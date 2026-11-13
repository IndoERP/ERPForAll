/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { addonsView } from "../src/client/services.js";
import { ClientError, RpcClient } from "../src/client/transport.js";
afterEach(() => document.body.replaceChildren());
// The catalog fields the panel needs; tests override what they exercise.
const row = (fields: Record<string, unknown>) => ({ summary: "", category: "Productivity", author: "ERPForAll", application: true, website: "", license: "LGPL-3", depends: [], ...fields });
test("addon confirmation owns the install operation and prevents refresh or duplicate submission", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  let release!: () => void, reads = 0, writes = 0;
  const gate = new Promise<void>(done => { release = done; }), jobs: Promise<void>[] = [], errors: unknown[] = [];
  const rpc = new RpcClient(async path => {
    if (String(path) === "/erp/module/install") { writes++; await gate; return new Response(JSON.stringify({ error: { message: "Install rejected", name: "erp.exceptions.UserError" } }), { status: 400, headers: { "Content-Type": "application/json" } }); }
    reads++;
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1", state: "uninstalled", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const controller = await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, () => {}, {});
  const install = [...host.querySelectorAll("button")].find(item => item.textContent === "Install")!;
  const refresh = [...host.querySelectorAll("button")].find(item => item.textContent === "Refresh catalog")!;
  install.click(); install.click(); refresh.click();
  expect(controller.busy()).toBe(true);
  expect(document.querySelectorAll("dialog")).toHaveLength(1);
  expect(install.disabled).toBe(true); expect(refresh.disabled).toBe(true); expect(reads).toBe(1);
  [...document.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button")].find(button => button.textContent === "Install")!.click();
  await vi.waitFor(() => expect(writes).toBe(1));
  refresh.click(); expect(reads).toBe(1); expect(controller.busy()).toBe(true);
  release(); await Promise.all(jobs);
  expect(writes).toBe(1); expect(reads).toBe(2); expect(errors).toHaveLength(1); expect(controller.busy()).toBe(false);
  const nextInstall = [...host.querySelectorAll("button")].find(item => item.textContent === "Install")!;
  nextInstall.click(); [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(item => item.textContent === "Cancel")!.click();
  await Promise.all(jobs); expect(controller.busy()).toBe(false); expect(writes).toBe(1);
  expect([...host.querySelectorAll("button")].every(item => !item.disabled)).toBe(true);
});

test("addons screen renders app-tile grid with name, summary, state badge, and Install/Uninstall buttons", async () => {
  const rpc = new RpcClient(async () => {
    return new Response(JSON.stringify({
      wire_major: 1,
      addons: [
        row({ name: "base", label: "Base", version: "1.0", state: "installed", installable: false, summary: "Core framework and base models" }),
        row({ name: "acceptance_notes", label: "Notes", version: "2.1", state: "uninstalled", installable: true, summary: "Collaborative note management" })
      ]
    }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const grid = host.querySelector(".addons-grid");
  expect(grid).not.toBeNull();

  const tiles = [...host.querySelectorAll(".addon-tile")];
  expect(tiles).toHaveLength(2);

  // First tile (installed)
  const baseTile = tiles[0]!;
  expect(baseTile.querySelector(".addon-icon")?.getAttribute("data-letter")).toBe("B");
  expect(baseTile.querySelector(".addon-name")?.textContent).toBe("Base");
  expect(baseTile.querySelector(".addon-technical-name")?.textContent).toBe("base");
  expect(baseTile.querySelector(".addon-badge-installed")?.textContent).toBe("Installed");
  expect(baseTile.querySelector(".addon-summary")?.textContent).toBe("Core framework and base models");
  expect(baseTile.querySelector(".addon-uninstall")?.textContent).toBe("Uninstall");
  expect(baseTile.querySelector(".addon-install")).toBeNull();

  // Second tile (uninstalled)
  const notesTile = tiles[1]!;
  expect(notesTile.querySelector(".addon-icon")?.getAttribute("data-letter")).toBe("N");
  expect(notesTile.querySelector(".addon-name")?.textContent).toBe("Notes");
  expect(notesTile.querySelector(".addon-badge-uninstalled")?.textContent).toBe("Not Installed");
  expect(notesTile.querySelector(".addon-summary")?.textContent).toBe("Collaborative note management");
  expect(notesTile.querySelector(".addon-install")?.textContent).toBe("Install");
  expect(notesTile.querySelector(".addon-uninstall")).toBeNull();
  // Every card offers Module info, and lists its version and author.
  expect(tiles.map(tile => tile.querySelector(".addon-info-btn")?.textContent)).toEqual(["Module info", "Module info"]);
  expect([...notesTile.querySelectorAll(".addon-facts dt, .addon-facts dd")].map(item => item.textContent)).toEqual(["Version", "2.1", "Author", "ERPForAll", "Category", "Productivity"]);
});

const catalog = (addons: unknown[]) => new RpcClient(async () => new Response(JSON.stringify({ wire_major: 1, addons }), { headers: { "Content-Type": "application/json" } }), () => {});

/// (partial: addon detail view renders manifest metadata, dependencies, and created menus/views with pagination)
test("addon detail view renders manifest metadata, dependencies, and created menus/views with pagination", async () => {
  let detailCalls = 0;
  const cursors: (string | undefined)[] = [];
  const rpc = new RpcClient(async (path, init) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      detailCalls++;
      const body = JSON.parse(String(init?.body || "{}"));
      cursors.push(body.dependencies_cursor);
      if (body.dependencies_cursor === "dep_cur_1") {
        return new Response(JSON.stringify({
          dependencies: { items: ["web"], next_cursor: null },
        }), { headers: { "Content-Type": "application/json" } });
      }
      return new Response(JSON.stringify({
        wire_major: 1,
        registry_version: "reg:1",
        name: "acceptance_notes",
        label: "Notes",
        summary: "Collaborative note management",
        description: "Full rich description of the Notes addon.",
        author: "ERPForAll",
        available_version: "2.1",
        installed_version: "2.1",
        state: "installed",
        dependencies: { items: ["base"], next_cursor: "dep_cur_1" },
        dependents: { items: ["sale_notes"], next_cursor: null },
        menus: { items: [{ id: 101, name: "notes_menu", label: "Notes App" }], next_cursor: null },
        views: { items: [{ id: 201, name: "notes.tree", model: "note.note", kind: "list" }], next_cursor: null },
        upgrade_refusal: null,
        uninstall_refusal: null,
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({
      wire_major: 1,
      addons: [row({ name: "acceptance_notes", label: "Notes", version: "2.1", state: "installed", installable: true, summary: "Collaborative note management" })]
    }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const tile = host.querySelector<HTMLElement>('[data-addon="acceptance_notes"]')!;
  tile.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());

  const detail = host.querySelector(".addon-detail")!;
  expect(detail).not.toBeNull();
  expect(detail.querySelector(".addon-detail-title")?.textContent).toBe("Notes");
  expect(detail.querySelector(".addon-technical-name")?.textContent).toBe("acceptance_notes");
  expect(detail.querySelector(".addon-detail-summary")?.textContent).toBe("Collaborative note management");
  expect(detail.querySelector(".addon-detail-description")?.textContent).toBe("Full rich description of the Notes addon.");

  const facts = Object.fromEntries([...detail.querySelectorAll(".addon-detail-facts dt")].map(dt => [dt.textContent, dt.nextElementSibling?.textContent]));
  expect(facts["Author"]).toBe("ERPForAll");
  expect(facts["Version"]).toBe("2.1");
  expect(facts["Installed version"]).toBe("2.1");

  const depSection = detail.querySelectorAll(".addon-depends")[0]!;
  expect(depSection.textContent).toContain("base");
  const moreBtn = detail.querySelector<HTMLButtonElement>(".addon-detail-more");
  expect(moreBtn).not.toBeNull();
  moreBtn!.click();
  await vi.waitFor(() => expect(detailCalls).toBe(2));
  expect(cursors).toContain("dep_cur_1");
  await vi.waitFor(() => expect(depSection.textContent).toContain("web"));

  expect(detail.textContent).toContain("Notes App");
  expect(detail.textContent).toContain("notes.tree");
});

/// (partial: addon detail back button restores catalog layout and focus)
test("addon detail back button restores catalog layout and focus", async () => {
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      return new Response(JSON.stringify({
        wire_major: 1, registry_version: "reg:1", name: "acceptance_notes", label: "Notes",
        summary: "Desc", description: "Desc", author: "A", available_version: "1", installed_version: "1",
        state: "installed", dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
        menus: { items: [], next_cursor: null }, views: { items: [], next_cursor: null },
        upgrade_refusal: null, uninstall_refusal: null
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({
      wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1", state: "installed", installable: true })]
    }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const tile = host.querySelector<HTMLElement>('[data-addon="acceptance_notes"]')!;
  tile.focus();
  tile.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());

  const backBtn = host.querySelector<HTMLButtonElement>(".addon-detail-back")!;
  backBtn.click();
  expect(host.querySelector(".addon-detail")).toBeNull();
  expect(host.querySelector(".addons-layout")).not.toBeNull();
  expect(document.activeElement).toBe(tile);
});

/// (partial: upgrade action requests explicit confirmation, calls /erp/module/upgrade, and reloads on success)
test("upgrade action requests explicit confirmation, calls /erp/module/upgrade, and reloads on success", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  let writes = 0, reloaded = false;
  Object.defineProperty(window, "location", { configurable: true, value: { reload: () => { reloaded = true; } } });

  const rpc = new RpcClient(async (path, init) => {
    if (String(path) === "/erp/module/upgrade") {
      writes++;
      const body = JSON.parse(String(init?.body || "{}"));
      expect(body.modules).toEqual(["acceptance_notes"]);
      return new Response(JSON.stringify({ upgraded: ["acceptance_notes"], installed: [], registry_version: "2", asset_version: "2" }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  const notices: string[] = [];
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, msg => notices.push(msg), {});

  const upgradeBtn = host.querySelector<HTMLButtonElement>(".addon-upgrade")!;
  upgradeBtn.click();

  const cancelBtn = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(b => b.textContent === "Cancel")!;
  cancelBtn.click();
  expect(writes).toBe(0);

  await vi.waitFor(() => expect(upgradeBtn.disabled).toBe(false));
  upgradeBtn.click();
  await vi.waitFor(() => expect([...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(b => b.textContent === "Upgrade")).toBeDefined());
  const confirmBtn = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(b => b.textContent === "Upgrade")!;
  confirmBtn.click();

  await vi.waitFor(() => expect(writes).toBe(1));
  expect(reloaded).toBe(true);
  expect(notices).toContain("Addon upgraded. Reloading the application…");
});

/// (partial: upgrade refusal in detail disables upgrade button and shows a fixed sentence)
test("upgrade refusal in detail disables upgrade button and shows a fixed sentence", async () => {
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      return new Response(JSON.stringify({
        wire_major: 1, registry_version: "reg:1", name: "acceptance_notes", label: "Notes",
        summary: "Desc", description: "Desc", author: "A", available_version: "1", installed_version: "1",
        state: "installed", dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
        menus: { items: [], next_cursor: null }, views: { items: [], next_cursor: null },
        upgrade_refusal: "Installed addon source is unavailable to native upgrade", uninstall_refusal: null
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({
      wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1", state: "installed", installable: true })]
    }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const tile = host.querySelector<HTMLElement>('[data-addon="acceptance_notes"]')!;
  tile.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());

  const upgradeBtn = host.querySelector<HTMLButtonElement>(".addon-detail .addon-upgrade")!;
  expect(upgradeBtn.disabled).toBe(true);
  const note = host.querySelector<HTMLElement>(".addon-refusal-note")!;
  expect(upgradeBtn.getAttribute("aria-describedby")).toBe(note.id);
  expect(upgradeBtn.title).toBe("");
  expect(note.textContent).toBe("Upgrade from this page is not available: this addon is not in the server's addon directory.");
  expect(note.dataset["reason"]).toBe("Installed addon source is unavailable to native upgrade");
});

/// (partial: detail page layout, one header row, fixed unavailable sentence, each menu once)
test("addon detail header, unavailable uninstall sentence and menu list", async () => {
  const internal = "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release";
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      return new Response(JSON.stringify({
        wire_major: 1, registry_version: "reg:1", name: "sample_library_with_a_long_technical_name", label: "Sample Library",
        summary: "S", description: "D", author: "A", available_version: "1", installed_version: "1", state: "installed",
        dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
        menus: { items: [{ id: 1, name: "Library", label: "Library" }, { id: 2, name: "library_books", label: "Books" }], next_cursor: null },
        views: { items: [], next_cursor: null }, upgrade_refusal: null, uninstall_refusal: internal,
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "sample_library_with_a_long_technical_name", label: "Sample Library", version: "1", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="sample_library_with_a_long_technical_name"]')!.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());
  const detail = host.querySelector(".addon-detail")!;

  const row1 = detail.querySelector(".addon-detail-title-row")!;
  expect([...row1.children].map(c => c.className)).toEqual(["addon-detail-title", "addon-badge addon-badge-installed"]);
  expect(detail.querySelector(".addon-technical-name")?.textContent).toBe("sample_library_with_a_long_technical_name");
  expect(detail.querySelector<HTMLElement>(".addon-technical-name")!.title).toBe("sample_library_with_a_long_technical_name");
  expect([...detail.querySelectorAll(".addon-detail-facts dt")].map(dt => dt.textContent)).not.toContain("State");

  const button = detail.querySelector<HTMLButtonElement>(".addon-uninstall")!;
  const note = detail.querySelector<HTMLElement>(`#${button.getAttribute("aria-describedby")}`)!;
  expect(button.disabled).toBe(true);
  expect(note.textContent).toBe("Uninstalling this addon from this page is not available yet.");
  expect(detail.textContent).not.toContain("a later release");
  expect(detail.textContent).not.toContain("lifecycle callbacks");

  const items = [...detail.querySelectorAll(".addon-metadata-item")].map(item => [...item.children].map(c => c.textContent));
  expect(items).toEqual([["Library"], ["Books", "library_books"]]);
});

/// (partial: detail page of an uninstalled addon that cannot be installed explains why)
test("addon detail of an uninstallable addon shows a calm note instead of actions", async () => {
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      return new Response(JSON.stringify({
        wire_major: 1, registry_version: "reg:1", name: "x_addon", label: "X", summary: "S", description: "D", author: "A",
        available_version: "1", installed_version: "", state: "uninstalled",
        dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
        menus: { items: [], next_cursor: null }, views: { items: [], next_cursor: null }, upgrade_refusal: null, uninstall_refusal: null,
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "x_addon", label: "X", version: "1", state: "uninstalled", installable: false })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="x_addon"]')!.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());
  expect(host.querySelector(".addon-detail .addon-install")).toBeNull();
  expect(host.querySelector(".addon-detail-notes")?.textContent).toContain("marked as not installable");
});

/// (partial: a refused action stays disabled after another operation ends)
test("a refused uninstall stays disabled after an upgrade is cancelled", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_addon_detail")) {
      return new Response(JSON.stringify({
        wire_major: 1, registry_version: "reg:1", name: "acceptance_notes", label: "Notes", summary: "S", description: "D", author: "A",
        available_version: "1", installed_version: "1", state: "installed",
        dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
        menus: { items: [], next_cursor: null }, views: { items: [], next_cursor: null }, upgrade_refusal: null,
        uninstall_refusal: "Uninstall impact cannot describe x; unsupported until a later release",
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="acceptance_notes"]')!.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail")).not.toBeNull());
  host.querySelector<HTMLButtonElement>(".addon-detail .addon-upgrade")!.click();
  const cancel = await vi.waitFor(() => { const b = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(x => x.textContent === "Cancel"); expect(b).toBeDefined(); return b!; });
  cancel.click();
  await vi.waitFor(() => expect(host.querySelector<HTMLButtonElement>(".addon-detail .addon-upgrade")!.disabled).toBe(false));
  expect(host.querySelector<HTMLButtonElement>(".addon-detail .addon-uninstall")!.disabled).toBe(true);
  expect(host.querySelector("#addon-uninstall-unavailable")?.textContent).toBe("Uninstalling this addon from this page is not available yet.");
});

/// (partial: uninstall preview refusal shows the fixed sentence and prevents confirmation dialog)
test("uninstall preview refusal shows the fixed sentence and prevents confirmation dialog", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });

  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_preview_uninstall")) {
      return new Response(JSON.stringify({ error: { message: "Native uninstall preview requires a trusted-session engine bridge", name: "erp.exceptions.UserError" } }), { status: 400, headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  const notices: string[] = [];
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, msg => notices.push(msg), {});

  const uninstallBtn = host.querySelector<HTMLButtonElement>(".addon-uninstall")!;
  uninstallBtn.click();

  await vi.waitFor(() => expect(notices.length).toBeGreaterThan(0));
  expect(notices[0]).toBe("Uninstalling this addon from this page is not available yet.");
  expect(document.querySelector(".addon-uninstall-dialog")).toBeNull();
});

/// (partial: any other uninstall preview failure still says it failed)
test("an uninstall preview server error is reported as a failure", async () => {
  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_preview_uninstall")) return new Response("{}", { status: 500 });
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const notices: string[] = [];
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, msg => notices.push(msg), {});
  host.querySelector<HTMLButtonElement>(".addon-uninstall")!.click();
  await vi.waitFor(() => expect(notices.length).toBeGreaterThan(0));
  expect(notices[0]).toContain("Uninstall preview failed");
});

/// (partial: uninstall confirmation dialog renders warning, affected modules, dropped models and exact record counts)
test("uninstall confirmation dialog renders warning, affected modules, dropped models and exact record counts", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });

  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_preview_uninstall")) {
      return new Response(JSON.stringify({
        version: 1,
        requested: ["acceptance_notes"],
        resolved: ["acceptance_notes", "notes_extra"],
        dropped_models: [
          { model: "note.note", table: "note_note", row_count: "42" },
          { model: "note.tag", table: "note_tag", row_count: "7" }
        ],
        metadata: [],
        target_count: "49",
        fingerprint: "fp_test_123"
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const uninstallBtn = host.querySelector<HTMLButtonElement>(".addon-uninstall")!;
  uninstallBtn.click();

  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull());
  const dialog = document.querySelector<HTMLDialogElement>(".addon-uninstall-dialog")!;
  expect(dialog.open).toBe(true);
  expect(dialog.querySelector(".addon-uninstall-title")?.textContent).toBe("Uninstall Notes?");
  expect(dialog.querySelector(".addon-uninstall-warning")?.textContent).toContain("Uninstalling modules can be risky");

  const mods = [...dialog.querySelectorAll(".addon-impact-modules li")].map(li => li.textContent);
  expect(mods).toEqual(["acceptance_notes", "notes_extra"]);

  const rows = [...dialog.querySelectorAll(".addon-impact-models-table tbody tr")].map(tr => [...tr.children].map(td => td.textContent));
  expect(rows).toEqual([
    ["note.note", "note_note", "42"],
    ["note.tag", "note_tag", "7"]
  ]);
  expect(dialog.querySelector(".addon-impact-summary")?.textContent).toContain("49");
});

/// (partial: uninstall confirmation dialog traps focus and restores focus on escape/cancel)
test("uninstall confirmation dialog traps focus and restores focus on escape/cancel", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });

  const rpc = new RpcClient(async (path) => {
    if (typeof path === "string" && path.includes("erp_preview_uninstall")) {
      return new Response(JSON.stringify({
        version: 1, requested: ["acceptance_notes"], resolved: ["acceptance_notes"],
        dropped_models: [], metadata: [], target_count: "0", fingerprint: "fp_test"
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});

  const uninstallBtn = host.querySelector<HTMLButtonElement>(".addon-uninstall")!;
  uninstallBtn.focus();
  uninstallBtn.click();

  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull());
  const dialog = document.querySelector<HTMLDialogElement>(".addon-uninstall-dialog")!;
  const cancelBtn = dialog.querySelector<HTMLButtonElement>(".addon-uninstall-cancel")!;
  const confirmBtn = dialog.querySelector<HTMLButtonElement>(".addon-uninstall-confirm")!;

  expect(document.activeElement).toBe(cancelBtn);

  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true }));
  expect(document.activeElement).toBe(confirmBtn);

  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  expect(document.querySelector(".addon-uninstall-dialog")).toBeNull();
  expect(document.activeElement).toBe(uninstallBtn);
});

/// (partial: confirmed uninstall submits fingerprint to /erp/module/uninstall and handles 409 conflict)
test("confirmed uninstall submits fingerprint to /erp/module/uninstall and handles 409 conflict", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });

  let uninstalls = 0, conflict = true;
  const notices: string[] = [];

  const rpc = new RpcClient(async (path, init) => {
    if (typeof path === "string" && path.includes("erp_preview_uninstall")) {
      return new Response(JSON.stringify({
        version: 1, requested: ["acceptance_notes"], resolved: ["acceptance_notes"],
        dropped_models: [], metadata: [], target_count: "0", fingerprint: "fp_123"
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (String(path) === "/erp/module/uninstall") {
      uninstalls++;
      const body = JSON.parse(String(init?.body || "{}"));
      expect(body.modules).toEqual(["acceptance_notes"]);
      expect(body.impact).toBe("fp_123");
      if (conflict) {
        conflict = false;
        return new Response(JSON.stringify({ message: "Database state changed", name: "erp.exceptions.Conflict" }), { status: 409, headers: { "Content-Type": "application/json" } });
      }
      return new Response(JSON.stringify({ uninstalled: ["acceptance_notes"], registry_version: "2", asset_version: "2" }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1.0", state: "installed", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, msg => notices.push(msg), {});

  const uninstallBtn = host.querySelector<HTMLButtonElement>(".addon-uninstall")!;
  uninstallBtn.click();
  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull());

  document.querySelector<HTMLButtonElement>(".addon-uninstall-confirm")!.click();
  await vi.waitFor(() => expect(uninstalls).toBe(1));
  await vi.waitFor(() => expect(notices).toContain("Database state changed since preview. A fresh confirmation is required."));
  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).toBeNull());

  await vi.waitFor(() => expect(host.querySelector<HTMLButtonElement>(".addon-uninstall")).not.toBeNull());
  host.querySelector<HTMLButtonElement>(".addon-uninstall")!.click();
  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull());
  document.querySelector<HTMLButtonElement>(".addon-uninstall-confirm")!.click();
  await vi.waitFor(() => expect(uninstalls).toBe(2));
  await vi.waitFor(() => expect(notices).toContain("Addon uninstalled. Reloading the application…"));
});

test("an unknown addon state is a loud server error", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const error = await addonsView(host, catalog([row({ name: "x", label: "X", version: "1", state: "bogus state", installable: true })]), job => { void job(); }, () => {}, {}).then(() => undefined, (caught: unknown) => caught);
  expect(error).toBeInstanceOf(ClientError);
  expect((error as ClientError).kind).toBe("server");
  expect((error as ClientError).message).toBe('Unknown addon state "bogus state" for addon x.');
});

test("a missing or non-string summary, category or flag is a loud server error", async () => {
  const cases: readonly (readonly [Record<string, unknown>, string])[] = [
    [{ summary: 5 }, "Invalid addon summary."], [{ summary: undefined }, "Invalid addon summary."],
    [{ category: 3 }, "Invalid addon category."], [{ author: null }, "Invalid addon author."],
    [{ application: "yes" }, "Invalid addon application flag."],
    [{ website: 1 }, "Invalid addon website."], [{ license: null }, "Invalid addon license."],
    [{ depends: "base" }, "Invalid addon depends."], [{ depends: ["base", 2] }, "Invalid addon dependency."],
  ];
  for (const [patch, message] of cases) {
    const host = document.createElement("div"); document.body.append(host);
    const error = await addonsView(host, catalog([row({ name: "x", label: "X", version: "1", state: "installed", installable: true, ...patch })]), job => { void job(); }, () => {}, {}).then(() => undefined, (caught: unknown) => caught);
    expect(error).toBeInstanceOf(ClientError);
    expect((error as ClientError).kind).toBe("server");
    expect((error as ClientError).message).toBe(message);
  }
  const ok = document.createElement("div"); document.body.append(ok);
  await addonsView(ok, catalog([row({ name: "x", label: "X", version: "1", state: "installed", installable: true })]), job => { void job(); }, () => {}, {});
  expect(ok.querySelector(".addon-summary")?.textContent).toBe("No description provided.");
});

test("each lifecycle state shows its own label and a fixed CSS token", async () => {
  const states = [["to upgrade", "To Upgrade", "to-upgrade"], ["to install", "To Install", "to-install"], ["to remove", "To Remove", "to-remove"], ["uninstallable", "Not Installable", "uninstallable"], ["unavailable", "Unavailable", "unavailable"]] as const;
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, catalog(states.map(([state], index) => row({ name: `m${index}`, label: `M${index}`, version: "1", state, installable: true }))), job => { void job(); }, () => {}, {});
  const badges = [...host.querySelectorAll(".addon-badge")];
  expect(badges.map(b => b.textContent)).toEqual(states.map(item => item[1]));
  expect(badges.map(b => b.className)).toEqual(states.map(item => `addon-badge addon-badge-${item[2]}`));
  expect(host.querySelectorAll(".addons-results .addon-install")).toHaveLength(0); // no Install for non-uninstalled states
});

test("a failed install call notifies, re-renders the catalog and rethrows the server error", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  let reads = 0; const notices: string[] = [], jobs: Promise<void>[] = [], errors: unknown[] = [];
  const rpc = new RpcClient(async path => {
    if (String(path) === "/erp/module/install") return new Response(JSON.stringify({ message: "Install rejected", name: "erp.exceptions.UserError" }), { status: 400, headers: { "Content-Type": "application/json" } });
    reads++;
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "acceptance_notes", label: "Notes", version: "1", state: "uninstalled", installable: true })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, message => { notices.push(message); }, {});
  [...host.querySelectorAll("button")].find(b => b.textContent === "Install")!.click();
  [...document.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button")].find(button => button.textContent === "Install")!.click();
  await vi.waitFor(() => expect(errors).toHaveLength(1));
  await Promise.all(jobs);
  expect(errors[0]).toBeInstanceOf(ClientError);
  expect((errors[0] as ClientError).kind).toBe("validation");
  expect((errors[0] as ClientError).message).toBe("Install rejected");
  expect(notices).toEqual(["Installation status may have changed. Reload the catalog before trying again."]);
  expect(reads).toBe(2);
});

const panelCatalog = () => catalog([
  row({ name: "sale", label: "Sales", version: "1", state: "installed", installable: true, category: "Sales/Sales", summary: "Quotations" }),
  row({ name: "sale_extra", label: "Sales Margin", version: "1", state: "uninstalled", installable: true, category: "Sales/Sales", application: false }),
  row({ name: "crm", label: "CRM", version: "1", state: "uninstalled", installable: true, category: "Sales/CRM", author: "Arkana" }),
  row({ name: "notes", label: "Notes", version: "1", state: "uninstalled", installable: true, category: "Productivity" }),
  row({ name: "bakery", label: "Bakery", version: "1", state: "uninstalled", installable: true, category: "Industries/Food/Bakery" }),
  row({ name: "base", label: "Base", version: "1", state: "installed", installable: false, category: "Hidden", application: false }),
]);
const tiles = (host: HTMLElement) => [...host.querySelectorAll<HTMLElement>(".addons-results .addon-tile")].map(tile => tile.dataset["addon"]);
const panelButton = (host: HTMLElement, label: string) => [...host.querySelectorAll<HTMLButtonElement>(".addons-panel-item")].find(item => item.firstChild?.textContent === label)!;

test("search panel: Apps filter on by default, Official/Industries sections and a category tree with counts", async () => {
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, panelCatalog(), job => { void job(); }, () => {}, {});
  expect(host.querySelector<HTMLButtonElement>('[data-filter="app"]')!.getAttribute("aria-pressed")).toBe("true");
  expect(tiles(host)).toEqual(["crm", "notes", "sale"]);
  expect(host.querySelector(".addons-count")?.textContent).toBe("3 addons");
  expect(panelButton(host, "Official Apps").getAttribute("aria-pressed")).toBe("true");
  expect(panelButton(host, "Official Apps").querySelector(".addons-panel-count")?.textContent).toBe("3");
  expect(panelButton(host, "Industries").querySelector(".addons-panel-count")?.textContent).toBe("1");
  // Top-level categories only until one is chosen; counts include descendants and respect the filters.
  expect([...host.querySelectorAll(".addons-panel-item")].map(item => item.firstChild?.textContent)).toEqual(["Official Apps", "Industries", "All", "Hidden", "Productivity", "Sales"]);
  expect(panelButton(host, "Sales").querySelector(".addons-panel-count")?.textContent).toBe("2");
  expect(panelButton(host, "Hidden").querySelector(".addons-panel-count")?.textContent).toBe("0");
  panelButton(host, "Sales").click();
  expect(tiles(host)).toEqual(["crm", "sale"]);
  expect(panelButton(host, "CRM").style.getPropertyValue("--depth")).toBe("1");
  panelButton(host, "CRM").click();
  expect(tiles(host)).toEqual(["crm"]);
  expect(panelButton(host, "CRM").getAttribute("aria-pressed")).toBe("true");
  panelButton(host, "All").click();
  expect(tiles(host)).toEqual(["crm", "notes", "sale"]);
  panelButton(host, "Industries").click();
  expect(tiles(host)).toEqual(["bakery"]);
  expect(host.querySelector(".addons-results .addon-category")?.textContent).toBe("Industries / Food / Bakery");
  expect([...host.querySelectorAll(".addons-panel-item")].map(item => item.firstChild?.textContent)).toEqual(["Official Apps", "Industries", "All", "Food"]);
});

test("filters combine OR within a group and AND across groups; search matches label, name and summary", async () => {
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, panelCatalog(), job => { void job(); }, () => {}, {});
  const filter = (key: string) => host.querySelector<HTMLButtonElement>(`[data-filter="${key}"]`)!;
  filter("extra").click();
  expect(tiles(host)).toEqual(["base", "crm", "notes", "sale", "sale_extra"]);
  filter("not_installed").click();
  expect(tiles(host)).toEqual(["crm", "notes", "sale_extra"]);
  filter("not_installed").click();
  filter("installed").click();
  expect(tiles(host)).toEqual(["base", "sale"]);
  filter("app").click();
  expect(tiles(host)).toEqual(["base"]);
  filter("installed").click(); filter("extra").click();
  expect([...host.querySelectorAll(".addons-filter.active")]).toHaveLength(0);
  const search = host.querySelector<HTMLInputElement>(".addons-search")!;
  search.value = "QUOTATION"; search.dispatchEvent(new Event("input"));
  expect(tiles(host)).toEqual(["sale"]);
  search.value = "sale_ext"; search.dispatchEvent(new Event("input"));
  expect(tiles(host)).toEqual(["sale_extra"]);
  search.value = "nothing here"; search.dispatchEvent(new Event("input"));
  expect(host.querySelector(".addons-empty")?.textContent).toBe("No addons match the current search, filters and category.");
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  expect(search.value).toBe("");
  expect(tiles(host)).toHaveLength(5);
});

test("group by category, status and author renders labelled sections", async () => {
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, panelCatalog(), job => { void job(); }, () => {}, {});
  const select = host.querySelector<HTMLSelectElement>(".addons-groupby")!;
  const groups = () => [...host.querySelectorAll(".addons-group-title")].map(title => title.textContent);
  select.value = "category"; select.dispatchEvent(new Event("change"));
  expect(groups()).toEqual(["Productivity (1)", "Sales / CRM (1)", "Sales / Sales (1)"]);
  select.value = "state"; select.dispatchEvent(new Event("change"));
  expect(groups()).toEqual(["Installed (1)", "Not Installed (2)"]);
  select.value = "author"; select.dispatchEvent(new Event("change"));
  expect(groups()).toEqual(["Arkana (1)", "ERPForAll (2)"]);
  select.value = ""; select.dispatchEvent(new Event("change"));
  expect(groups()).toEqual([]);
  expect(tiles(host)).toEqual(["crm", "notes", "sale"]);
});

test("an installed extension addon from a WebAssembly package is listed with its technical name", async () => {
  const rpc = new RpcClient(async () => new Response(JSON.stringify({
    wire_major: 1,
    addons: [
      row({ name: "sample_library", label: "Sample Library", version: "1.0.0", state: "installed", installable: true, category: "Sample" }),
      row({ name: "sample_library_wasm", label: "Sample Library WASM", version: "1.2.0", state: "installed", installable: false, category: "Sample", application: false }),
    ],
  }), { headers: { "Content-Type": "application/json" } }), () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});
  const names = (): string[] => [...host.querySelectorAll(".addon-technical-name")].map(item => item.textContent ?? "");
  expect(names()).toEqual(["sample_library"]);
  [...host.querySelectorAll<HTMLButtonElement>(".addons-filter")].find(item => item.dataset["filter"] === "app")!.click();
  expect(names()).toEqual(["sample_library", "sample_library_wasm"]);
  const search = host.querySelector<HTMLInputElement>(".addons-search")!;
  search.value = "library_wasm"; search.dispatchEvent(new Event("input"));
  expect(names()).toEqual(["sample_library_wasm"]);
  const wasm = host.querySelector('[data-addon="sample_library_wasm"]')!;
  expect(wasm.querySelector(".addon-badge")?.textContent).toBe("Installed");
  expect([...wasm.querySelectorAll("button")].some(item => item.textContent === "Install")).toBe(false);
});

test("Module info opens a read-only dialog with the manifest details and returns focus to the card", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });
  let calls = 0;
  const rpc = new RpcClient(async () => {
    calls++;
    return new Response(JSON.stringify({ wire_major: 1, addons: [
      row({ name: "sample_library", label: "Sample Library", version: "1.0.0", state: "installed", installable: true, category: "Sample", author: "Arkana", summary: "Books and loans", website: "https://example.com/library", license: "LGPL-3", depends: ["base", "web"] }),
      row({ name: "notes", label: "Notes", version: "2.0", state: "uninstalled", installable: true, website: "javascript:alert(1)", license: "", application: false }),
    ] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job(); }, () => {}, {});
  const info = host.querySelector<HTMLButtonElement>('[data-addon="sample_library"] .addon-info-btn')!;
  info.click();
  const dialog = document.querySelector<HTMLDialogElement>("dialog.addon-info-dialog")!;
  expect(dialog.open).toBe(true);
  expect(dialog.querySelector("#" + dialog.getAttribute("aria-labelledby"))?.textContent).toBe("Sample Library");
  const facts = Object.fromEntries([...dialog.querySelectorAll("dt")].map(term => [term.textContent, term.nextElementSibling?.textContent]));
  expect(facts).toEqual({ "Technical name": "sample_library", Version: "1.0.0", Category: "Sample", Type: "App", Author: "Arkana", License: "LGPL-3", Website: "example.com/library", "Depends on": "baseweb", Installable: "Yes" });
  const link = dialog.querySelector<HTMLAnchorElement>("a.addon-info-link")!;
  expect(link.href).toBe("https://example.com/library"); expect(link.rel).toBe("noopener noreferrer");
  expect(dialog.querySelector(".addon-info-note")?.textContent).toContain("");
  expect(calls).toBe(1);
  [...dialog.querySelectorAll("button")].find(item => item.textContent === "Close")!.click();
  expect(document.querySelector("dialog.addon-info-dialog")).toBeNull();
  expect(document.activeElement).toBe(info);
  // A website that is not http(s) is shown as text, never as a link; empty values show a dash.
  [...host.querySelectorAll<HTMLButtonElement>(".addons-filter")].find(item => item.dataset["filter"] === "app")!.click();
  host.querySelector<HTMLButtonElement>('[data-addon="notes"] .addon-info-btn')!.click();
  const other = document.querySelector<HTMLDialogElement>("dialog.addon-info-dialog")!;
  expect(other.querySelector("a")).toBeNull();
  const otherFacts = Object.fromEntries([...other.querySelectorAll("dt")].map(term => [term.textContent, term.nextElementSibling?.textContent]));
  expect(otherFacts["Website"]).toBe("javascript:alert(1)"); expect(otherFacts["License"]).toBe("—"); expect(otherFacts["Depends on"]).toBe("None"); expect(otherFacts["Type"]).toBe("Extra");
  expect(other.querySelector(".addon-info-note")).toBeNull();
});

test("Load demo data asks for the irreversible confirmation and posts confirm=true only after it", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; } });
  const posts: { path: string; body: unknown }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    if (String(path) === "/erp/module/force-demo") {
      posts.push({ path: String(path), body: JSON.parse(String((init as RequestInit).body)) });
      return new Response(JSON.stringify({ error: { message: "Demo rejected", name: "erp.exceptions.UserError" } }), { status: 400, headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({ wire_major: 1, addons: [row({ name: "base", label: "Base", version: "1", state: "installed", installable: false })] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const jobs: Promise<void>[] = [], errors: unknown[] = [];
  await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, () => {}, {});
  const load = () => [...host.querySelectorAll("button")].find(item => item.textContent === "Load demo data")!;
  const answer = (label: string) => [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(item => item.textContent === label)!.click();
  load().click();
  expect(document.querySelector("dialog")?.textContent).toContain("irreversibly");
  answer("Oops, no!");
  await Promise.all(jobs);
  expect(posts).toHaveLength(0);
  load().click();
  answer("Yes, I understand the risks");
  await vi.waitFor(() => expect(posts).toHaveLength(1));
  await Promise.all(jobs);
  expect(posts[0]).toEqual({ path: "/erp/module/force-demo", body: { confirm: true } });
  expect(errors).toHaveLength(1);
});

// Review S2/S3 (cycle 4): strict decoding, reported Load more failures, stale detail replies, Escape during uninstall.
const detailDoc = (name: string, over: Record<string, unknown> = {}) => ({
  wire_major: 1, registry_version: "reg:1", name, label: name.toUpperCase(), summary: "S", description: "D", author: "A", available_version: "1", installed_version: "1",
  state: "installed", dependencies: { items: [], next_cursor: null }, dependents: { items: [], next_cursor: null },
  menus: { items: [], next_cursor: null }, views: { items: [], next_cursor: null }, upgrade_refusal: null, uninstall_refusal: null, ...over });
const json = (body: unknown) => new Response(JSON.stringify(body), { headers: { "Content-Type": "application/json" } });
const twoAddons = { wire_major: 1, addons: ["aa", "bb"].map(name => row({ name, label: name, version: "1", state: "installed", installable: true })) };
test("an addon detail missing a required field is an error naming the field, not an empty value", async () => {
  const rpc = new RpcClient(async path => String(path).includes("erp_addon_detail") ? json({ ...detailDoc("aa"), description: undefined }) : json(twoAddons), () => {});
  const host = document.createElement("div"); document.body.append(host);
  const errors: unknown[] = [], jobs: Promise<void>[] = [];
  await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="aa"]')!.click(); await Promise.all(jobs);
  expect(String((errors[0] as Error).message)).toContain("addon detail description"); expect(host.querySelector(".addon-detail")).toBeNull();
});
test("a failed Load more request goes through run and is reported", async () => {
  const rpc = new RpcClient(async (path, init) => {
    if (!String(path).includes("erp_addon_detail")) return json(twoAddons);
    return JSON.parse(String(init?.body)).dependencies_cursor ? new Response("{}", { status: 500 }) : json(detailDoc("aa", { dependencies: { items: ["base"], next_cursor: "c1" } }));
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const errors: unknown[] = [], jobs: Promise<void>[] = [];
  await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="aa"]')!.click(); await Promise.all(jobs);
  host.querySelector<HTMLButtonElement>(".addon-detail-more")!.click(); await Promise.all(jobs);
  expect(errors).toHaveLength(1); expect(host.querySelector<HTMLButtonElement>(".addon-detail-more")!.disabled).toBe(false);
});
test("a slow detail reply for an earlier click does not replace the later addon's page", async () => {
  let releaseA!: () => void; const gateA = new Promise<void>(done => { releaseA = done; });
  const rpc = new RpcClient(async (path, init) => {
    if (!String(path).includes("erp_addon_detail")) return json(twoAddons);
    const name = JSON.parse(String(init?.body)).name as string;
    if (name === "aa") await gateA;
    return json(detailDoc(name));
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const jobs: Promise<void>[] = [];
  await addonsView(host, rpc, job => { jobs.push(job()); }, () => {}, {});
  host.querySelector<HTMLElement>('[data-addon="aa"]')!.click();
  host.querySelector<HTMLElement>('[data-addon="bb"]')!.click();
  await vi.waitFor(() => expect(host.querySelector(".addon-detail-title")?.textContent).toBe("BB"));
  releaseA(); await Promise.all(jobs);
  expect(host.querySelector(".addon-detail-title")?.textContent).toBe("BB");
});
test("an uninstall preview without its fingerprint is an error and no dialog opens", async () => {
  const rpc = new RpcClient(async path => String(path).includes("erp_preview_uninstall")
    ? json({ version: 1, requested: ["aa"], resolved: ["aa"], dropped_models: [], metadata: [], target_count: "0" }) : json(twoAddons), () => {});
  const host = document.createElement("div"); document.body.append(host);
  const errors: unknown[] = [], jobs: Promise<void>[] = [];
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  await addonsView(host, rpc, job => { jobs.push(job().catch(error => { errors.push(error); })); }, () => {}, {});
  host.querySelector<HTMLButtonElement>(".addon-uninstall")!.click(); await Promise.all(jobs);
  expect(String((errors[0] as Error).message)).toContain("fingerprint"); expect(document.querySelector(".addon-uninstall-dialog")).toBeNull();
});
test("Escape and the cancel event are ignored while the uninstall request runs", async () => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value: function (this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value: function (this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });
  let release!: () => void, posts = 0; const gate = new Promise<void>(done => { release = done; });
  const rpc = new RpcClient(async path => {
    if (String(path).includes("erp_preview_uninstall")) return json({ version: 1, requested: ["aa"], resolved: ["aa"], dropped_models: [], metadata: [], target_count: "0", fingerprint: "fp" });
    if (String(path) === "/erp/module/uninstall") { posts++; await gate; return new Response("{}", { status: 500 }); }
    return json(twoAddons);
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await addonsView(host, rpc, job => { void job().catch(() => {}); }, () => {}, {});
  host.querySelector<HTMLButtonElement>(".addon-uninstall")!.click();
  await vi.waitFor(() => expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull());
  const dialog = document.querySelector<HTMLDialogElement>(".addon-uninstall-dialog")!;
  document.querySelector<HTMLButtonElement>(".addon-uninstall-confirm")!.click();
  await vi.waitFor(() => expect(posts).toBe(1));
  dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  const cancel = new Event("cancel", { cancelable: true }); dialog.dispatchEvent(cancel);
  expect(cancel.defaultPrevented).toBe(true); expect(document.querySelector(".addon-uninstall-dialog")).not.toBeNull();
  release();
});

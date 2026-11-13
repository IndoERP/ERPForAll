/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { beforeEach, expect, test, vi } from "vitest";
import { HumanApp } from "../src/client/app.js";

beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
});

function createHost(): HTMLElement {
  const hostEl = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    hostEl.append(node);
  }
  document.body.append(hostEl);
  return hostEl;
}

const baseBootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [{ id: 1, name: "Main Corp", parent_id: null }],
  current_company_id: 1,
  allowed_company_ids: [1],
  active_company_ids: [1],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token" },
};

const sampleMenus = {
  wire_major: 1,
  registry_version: "r1",
  roots: [1, 2, 3],
  nodes: [
    { module: null, id: 1, label: "Sales", parent_id: null, action_id: 101, children: [], icon: "lucide:shopping-bag" },
    { module: null, id: 2, label: "Inventory", parent_id: null, action_id: 102, children: [], icon: "lucide:box" },
    { module: null, id: 3, label: "Accounting", parent_id: null, action_id: 103, children: [], icon: "lucide:calculator" },
  ],
};

function setupOrderFetch(initialConfig: string | null = null, savedCalls?: any[]): void {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("erp_load_web_menus")) {
      return new Response(JSON.stringify(sampleMenus), { headers: { "Content-Type": "application/json" } });
    }
    if (url.includes("get_res_users_settings")) {
      return new Response(JSON.stringify({
        homemenu_config: initialConfig,
      }), { headers: { "Content-Type": "application/json" } });
    }
    if (url.includes("set_res_users_settings")) {
      const body = init?.body ? JSON.parse(String(init.body)) : {};
      if (savedCalls) savedCalls.push(body);
      return new Response(JSON.stringify({ success: true }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response("[]", { headers: { "Content-Type": "application/json" } });
  }));
}

test("initial home menu ordering restores custom homemenu_config from res.users.settings", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch("[3, 1, 2]");

  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  expect((app as any).rootMenuIds).toEqual([3, 1, 2]);

  const tiles = [...root.querySelectorAll<HTMLElement>(".home-grid .app-tile")];
  expect(tiles.map(t => t.dataset["menuId"])).toEqual(["3", "1", "2"]);

  app.dispose();
  root.remove();
});

test("drag and drop reorders tiles in home menu and persists to res.users.settings via RPC", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  const savedCalls: any[] = [];
  setupOrderFetch("[1, 2, 3]", savedCalls);

  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  const grid = root.querySelector<HTMLElement>(".home-grid")!;
  const tiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
  const tile2 = tiles[1]!; // id 2
  const tile3 = tiles[2]!; // id 3

  // Simulate drag transfer
  let transferredData = "";
  const dataTransfer: any = {
    effectAllowed: "none",
    dropEffect: "none",
    setData: (_format: string, data: string) => { transferredData = data; },
    getData: (_format: string) => transferredData,
  };

  const dragStartEvent = new Event("dragstart", { bubbles: true }) as any;
  dragStartEvent.dataTransfer = dataTransfer;
  tile2.dispatchEvent(dragStartEvent);
  expect(transferredData).toBe("2");
  expect(tile2.classList.contains("app-tile-dragged")).toBe(true);
  expect(tile2.classList.contains("o_dragged_app")).toBe(false);

  const dragOverEvent = new Event("dragover", { bubbles: true, cancelable: true }) as any;
  dragOverEvent.dataTransfer = dataTransfer;
  tile3.dispatchEvent(dragOverEvent);

  const dropEvent = new Event("drop", { bubbles: true, cancelable: true }) as any;
  dropEvent.dataTransfer = dataTransfer;
  tile3.dispatchEvent(dropEvent);

  // Dropping tile 2 onto tile 3 reorders [1, 2, 3] -> [1, 3, 2]
  await vi.waitFor(() => {
    const updatedTiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
    expect(updatedTiles.map(t => t.dataset["menuId"])).toEqual(["1", "3", "2"]);
  });

  expect((app as any).rootMenuIds).toEqual([1, 3, 2]);
  expect(savedCalls.length).toBeGreaterThan(0);
  const lastCall = savedCalls[savedCalls.length - 1];
  expect(lastCall.new_settings.homemenu_config).toBe("[1,3,2]");

  app.dispose();
  root.remove();
});

test("programmatic reorderApps updates rootMenuIds and calls set_res_users_settings", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  const savedCalls: any[] = [];
  setupOrderFetch("[1, 2, 3]", savedCalls);

  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  await app.reorderApps([2, 1, 3]);

  expect((app as any).rootMenuIds).toEqual([2, 1, 3]);
  expect(savedCalls.length).toBeGreaterThan(0);
  const call = savedCalls[savedCalls.length - 1];
  expect(call.new_settings.homemenu_config).toBe("[2,1,3]");

  app.dispose();
  root.remove();
});

test("missing or empty homemenu_config falls back to default menu order without error", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch(null);

  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  expect((app as any).rootMenuIds).toEqual([1, 2, 3]);

  const tiles = [...root.querySelectorAll<HTMLElement>(".home-grid .app-tile")];
  expect(tiles.map(t => t.dataset["menuId"])).toEqual(["1", "2", "3"]);

  app.dispose();
  root.remove();
});

test("dragging tile toggles app-tile-dragged class on dragstart and dragend", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch("[1, 2, 3]");

  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  const tile = root.querySelector<HTMLElement>(".home-grid .app-tile")!;
  const dataTransfer: any = {
    effectAllowed: "none",
    dropEffect: "none",
    setData: () => {},
    getData: () => "",
  };

  const dragStartEvent = new Event("dragstart", { bubbles: true }) as any;
  dragStartEvent.dataTransfer = dataTransfer;
  tile.dispatchEvent(dragStartEvent);
  expect(tile.classList.contains("app-tile-dragged")).toBe(true);
  expect(tile.classList.contains("o_dragged_app")).toBe(false);

  const dragEndEvent = new Event("dragend", { bubbles: true }) as any;
  tile.dispatchEvent(dragEndEvent);
  expect(tile.classList.contains("app-tile-dragged")).toBe(false);

  app.dispose();
  root.remove();
});

/// (partial: a corrupt saved order is reported, the default order stays)
test("a corrupt homemenu_config is reported through notifications and the default order is kept", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch("{not json");
  const app = new HumanApp(root, baseBootstrap);
  await app.start();
  expect((app as any).rootMenuIds).toEqual([1, 2, 3]);
  expect(document.body.textContent).toContain("Your saved app order could not be applied");
  app.dispose();
  root.remove();
});

/// (partial: reorderApps uses write: true and preserves order on RPC failure)
test("reorderApps does not change rootMenuIds on RPC failure and calls with write true", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch("[1, 2, 3]");
  const app = new HumanApp(root, baseBootstrap);
  await app.start();
  expect((app as any).rootMenuIds).toEqual([1, 2, 3]);

  const callSpy = vi.spyOn((app as any).rpc, "call").mockRejectedValueOnce(new Error("RPC failed"));

  await expect(app.reorderApps([2, 1, 3])).rejects.toThrow("RPC failed");

  expect(callSpy).toHaveBeenCalledWith(
    "res.users.settings",
    "set_res_users_settings",
    expect.objectContaining({
      new_settings: { homemenu_config: "[2,1,3]" },
    }),
    { write: true }
  );
  expect((app as any).rootMenuIds).toEqual([1, 2, 3]);

  app.dispose();
  root.remove();
});

/// (partial: keyboard shortcuts reorder tiles and keep focus)
test("keyboard shortcuts (Ctrl+Arrow/Alt+Arrow) move app tile, persist order and keep focus", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  const savedCalls: any[] = [];
  setupOrderFetch("[1, 2, 3]", savedCalls);
  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  const grid = root.querySelector<HTMLElement>(".home-grid")!;
  const tile2 = grid.querySelector<HTMLElement>('.app-tile[data-menu-id="2"]')!;
  tile2.focus();

  // Move tile 2 left (backward) using Ctrl+ArrowLeft
  tile2.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, bubbles: true, cancelable: true }));

  await vi.waitFor(() => {
    const updatedTiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
    expect(updatedTiles.map(t => t.dataset["menuId"])).toEqual(["2", "1", "3"]);
  });

  expect((app as any).rootMenuIds).toEqual([2, 1, 3]);
  expect(document.activeElement?.getAttribute("data-menu-id")).toBe("2");

  // Move tile 2 right (forward) using Alt+ArrowRight
  const newTile2 = grid.querySelector<HTMLElement>('.app-tile[data-menu-id="2"]')!;
  newTile2.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", altKey: true, bubbles: true, cancelable: true }));

  await vi.waitFor(() => {
    const updatedTiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
    expect(updatedTiles.map(t => t.dataset["menuId"])).toEqual(["1", "2", "3"]);
  });

  expect((app as any).rootMenuIds).toEqual([1, 2, 3]);
  expect(document.activeElement?.getAttribute("data-menu-id")).toBe("2");

  app.dispose();
  root.remove();
});

/// (partial: home tile reorder updates optimistically and rolls back on failure)
test("home tile reorder updates rootMenuIds and grid optimistically and rolls back on RPC failure", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupOrderFetch("[1, 2, 3]");
  const app = new HumanApp(root, baseBootstrap);
  await app.start();

  let rejectRpc!: (err: Error) => void;
  let rpcCalled = false;
  const pendingRpc = new Promise<never>((_, reject) => {
    rejectRpc = reject;
  });

  vi.spyOn((app as any).rpc, "call").mockImplementation(((model: unknown, method: unknown) => {
    if (model === "res.users.settings" && method === "set_res_users_settings") {
      rpcCalled = true;
      return pendingRpc;
    }
    return Promise.resolve();
  }) as never);

  const grid = root.querySelector<HTMLElement>(".home-grid")!;
  const tile2 = grid.querySelector<HTMLElement>('.app-tile[data-menu-id="2"]')!;
  tile2.focus();

  // Move tile 2 left (backward) using Ctrl+ArrowLeft
  tile2.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", ctrlKey: true, bubbles: true, cancelable: true }));

  // Optimistic update: rootMenuIds and DOM order update immediately while RPC is still pending!
  expect(rpcCalled).toBe(true);
  expect((app as any).rootMenuIds).toEqual([2, 1, 3]);
  const optimisticTiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
  expect(optimisticTiles.map(t => t.dataset["menuId"])).toEqual(["2", "1", "3"]);

  // Fail the RPC
  rejectRpc(new Error("network error"));
  await vi.waitFor(() => {
    // Rolled back to original order on failure
    expect((app as any).rootMenuIds).toEqual([1, 2, 3]);
    const rolledBackTiles = [...grid.querySelectorAll<HTMLElement>(".app-tile")];
    expect(rolledBackTiles.map(t => t.dataset["menuId"])).toEqual(["1", "2", "3"]);
  });

  app.dispose();
  root.remove();
});


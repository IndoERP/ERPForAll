/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { MessagingStore } from "@erpforall/mail-store";
import { renderDiscuss } from "../src/client/discuss.js";
import { ClientError, type RpcClient } from "../src/client/transport.js";

const mounts = vi.hoisted(() => [] as { store: unknown; id: number; disposed: boolean }[]);
vi.mock("../src/client/chatter.js", () => ({
  Chatter: class {
    mounted: { store: unknown; id: number; disposed: boolean };
    constructor(options: { store: unknown; recordId: number; host: HTMLElement }) {
      this.mounted = { store: options.store, id: options.recordId, disposed: false };
      mounts.push(this.mounted);
      options.host.textContent = "Conversation messages";
    }
    refresh() {
      return Promise.resolve();
    }
    dispose() {
      this.mounted.disposed = true;
    }
  },
}));
class Socket extends EventTarget {
  static all: Socket[] = [];
  constructor() {
    super();
    Socket.all.push(this);
  }
  send() {}
  close() {}
  replay() {
    this.dispatchEvent(new MessageEvent("message", { data: '{"type":"resync"}' }));
  }
}
class Bus {
  static listeners = new Set<Bus>();
  constructor() {
    Bus.listeners.add(this);
  }
  onmessage?: (e: { data: unknown }) => void;
  postMessage(data: unknown) {
    for (const other of Bus.listeners) if (other !== this) other.onmessage?.({ data });
  }
  close() {
    Bus.listeners.delete(this);
  }
}
function find(root: HTMLElement, text: string) {
  const found = Array.from(root.querySelectorAll("button")).find((b) => b.textContent === text);
  if (!found) throw new Error(`Missing button ${text}`);
  return found;
}
afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
  document.body.replaceChildren();
  mounts.length = 0;
  Socket.all = [];
  Bus.listeners.clear();
});
test("two tabs receive only wakes, reconcile duplicate deliveries, reconnect and clear session data", async () => {
  vi.stubGlobal("WebSocket", Socket);
  vi.stubGlobal("BroadcastChannel", Bus);
  const channel = (id: number, name: string) => ({
    id,
    name,
    latest: null,
    unread: 0,
    bookmarks: 0,
    seen: 0,
    owner: true,
  });
  let channels = [channel(2, "Planning")];
  let expired = false;
  const call = vi.fn(async (_model: string, method: string, args: Record<string, unknown>) => {
    if (expired) throw new ClientError("session", "Sign in again.");
    if (method === "conversation_create") {
      channels.push(channel(3, String(args["name"])));
      return 3;
    }
    if (method === "conversation_list") return channels.map((c) => ({ ...c }));
    throw new Error(`Unexpected ${method}`);
  });
  function tab() {
    const store = new MessagingStore({
      reportError: (e) => {
        throw e;
      },
    });
    const handle = renderDiscuss({
      rpc: { call } as unknown as RpcClient,
      store,
      uid: 5,
      userName: "A",
      policy: { company: 1, csrf: "test", retry: [1, 4] },
    });
    document.body.append(handle.node);
    find(handle.node, "Discuss").click();
    return { ...handle, store };
  }
  const first = tab(),
    second = tab();
  await vi.waitFor(() => expect(second.node.textContent).toContain("Planning"));
  const input = first.node.querySelector<HTMLInputElement>('[aria-label="Conversation name"]')!;
  input.value = "Purchasing";
  find(first.node, "Create conversation").click();
  await vi.waitFor(() => expect(second.node.textContent).toContain("Purchasing"));
  for (const s of Socket.all) {
    s.replay();
    s.replay();
  }
  await vi.waitFor(() => expect(second.store.local("DiscussSummary").ids()).toEqual(["2", "3"]));
  channels = [{ ...channel(3, "Purchasing"), unread: 1000 }];
  Socket.all[1]!.dispatchEvent(new CloseEvent("close", { code: 1006 }));
  await vi.waitFor(() => expect(Socket.all.length).toBe(3));
  Socket.all[2]!.replay();
  await vi.waitFor(() => expect(second.node.textContent).toContain("999+ unread"));
  expect(second.node.textContent).not.toContain("Planning");
  expired = true;
  Socket.all[2]!.replay();
  Socket.all[0]!.dispatchEvent(new CloseEvent("close", { code: 1008 }));
  await vi.waitFor(() => expect(second.node.textContent).toContain("Sign in again."));
  expect(second.store.local("DiscussSummary").ids()).toEqual([]);
  expect(first.store.local("DiscussSummary").ids()).toEqual([]);
  expect(second.node.textContent).not.toContain("Purchasing");
  first.dispose();
  second.dispose();
  expect(Bus.listeners.size).toBe(0);
});

test("Discuss, chat windows and messaging tabs share records and release them on departure", async () => {
  vi.stubGlobal("WebSocket", Socket);
  vi.stubGlobal("BroadcastChannel", Bus);
  const entry = (id: number, name: string) => ({
    id,
    name,
    owner: true,
    unread: 0,
    bookmarks: 0,
    seen: 0,
    latest: 12,
  });
  let page = [entry(7, "Sales"), entry(9, "Warehouse")];
  const call = vi.fn(async (_: string, method: string) => {
    if (method === "conversation_list") return page;
    if (method === "conversation_receipts")
      return [
        { user: 5, seen: 12 },
        { user: 9, seen: 0 },
      ];
    if (method === "conversation_leave") {
      page = page.filter((row) => row.id !== 7);
      return null;
    }
    throw new Error("Unexpected method");
  });
  const store = new MessagingStore({
    reportError: (e) => {
      throw e;
    },
  });
  const view = renderDiscuss({
    store,
    rpc: { call } as unknown as RpcClient,
    policy: { company: 1, csrf: "test", retry: [1, 4] },
    uid: 5,
    userName: "A",
  });
  document.body.append(view.node);
  find(view.node, "Discuss").click();
  await vi.waitFor(() => expect(view.node.textContent).toContain("Sales"));
  find(view.node, "Sales · 0 unread · 0 saved").click();
  find(view.node, "Read receipts").click();
  await vi.waitFor(() => expect(view.node.textContent).toContain("1 of 2 participants"));
  find(view.node, "Open chat window").click();
  find(view.node, "Warehouse · 0 unread · 0 saved").click();
  find(view.node, "Open chat window").click();
  expect(view.node.querySelectorAll(".discuss-window")).toHaveLength(2);
  expect(mounts.every((m) => m.store === store)).toBe(true);
  find(view.node, "Sales").click();
  find(view.node, "Leave conversation").click();
  await vi.waitFor(() => expect(view.node.querySelectorAll(".discuss-window")).toHaveLength(1));
  expect(store.local("DiscussSummary").ids()).toEqual(["9"]);
  expect(mounts.filter((m) => m.id === 7).every((m) => m.disposed)).toBe(true);
  view.dispose();
  expect(mounts.every((m) => m.disposed)).toBe(true);
});

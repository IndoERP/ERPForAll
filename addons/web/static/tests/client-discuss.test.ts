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
  const found = Array.from(root.querySelectorAll("button")).find((b) => b.getAttribute("aria-label") === text || b.textContent === text);
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
    owner: true, channel_type: "group", date: null, preview: "", sender: null,
  });
  let channels = [channel(2, "Planning")];
  let expired = false;
  const call = vi.fn(async (_model: string, method: string, args: Record<string, unknown>) => {
    if (expired) throw new ClientError("session", "Sign in again.");
    if (method === "conversation_people") return [{ id: 9, name: "Purchasing" }];
    if (method === "conversation_summary") return channels.find(row => row.id === (args["ids"] as number[])[0]) ?? null;
    if (method === "conversation_start") {
      channels.push(channel(3, "Purchasing"));
      return 3;
    }
    if (method === "conversation_search") return { items: channels.map((c) => ({ ...c })), next: null };
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
    const trigger = find(handle.node, "Discuss");
    expect(trigger.querySelector("svg[aria-hidden=true]")?.getAttribute("data-icon")).toBe("messages-square");
    expect(trigger.getAttribute("aria-label")).toBe("Discuss");
    trigger.click();
    return { ...handle, store };
  }
  const first = tab(),
    second = tab();
  await vi.waitFor(() => expect(second.node.textContent).toContain("Planning"));
  find(first.node, "New message").click();
  await vi.waitFor(() => find(first.node, "Purchasing"));
  find(first.node, "Purchasing").click();
  find(first.node, "Start conversation").click();
  await vi.waitFor(() => expect(second.node.textContent).toContain("Purchasing"));
  for (const s of Socket.all) {
    s.replay();
    s.replay();
  }
  await vi.waitFor(() => expect(second.store.local("DiscussSummary").ids()).toEqual(["2", "3"]));
  channels = [{ ...channel(3, "Purchasing"), unread: 1000 }];
  const socketCount = Socket.all.length;
  Socket.all[1]!.dispatchEvent(new CloseEvent("close", { code: 1006 }));
  await vi.waitFor(() => expect(Socket.all.length).toBe(socketCount + 1));
  const recovered = Socket.all.at(-1)!;
  recovered.replay();
  await vi.waitFor(() => expect(second.node.textContent).toContain("999+"));
  expect(second.node.textContent).not.toContain("Planning");
  expired = true;
  recovered.replay();
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
    owner: true, channel_type: "group", date: null, preview: "", sender: null,
    unread: 0,
    bookmarks: 0,
    seen: 0,
    latest: 12,
  });
  let page = [entry(7, "Sales"), entry(9, "Warehouse")];
  const call = vi.fn(async (_: string, method: string, args: Record<string, unknown>) => {
    if (method === "conversation_search") return { items: page, next: null };
    if (method === "conversation_summary") return page.find(c => c.id === (args["ids"] as number[])[0]) ?? null;
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
  const choose = (id: number) => view.node.querySelector<HTMLButtonElement>(`button[data-id="${id}"]`)!.click();
  choose(7);
  find(view.node, "Open chat window").click();
  choose(9);
  find(view.node, "Open chat window").click();
  expect(view.node.querySelectorAll(".discuss-window")).toHaveLength(2);
  expect(mounts.every((m) => m.store === store)).toBe(true);
  choose(7);
  find(view.node, "Leave conversation").click();
  await vi.waitFor(() => expect(view.node.querySelectorAll(".discuss-window")).toHaveLength(1));
  expect(store.local("DiscussSummary").ids()).toEqual(["9"]);
  expect(mounts.filter((m) => m.id === 7).every((m) => m.disposed)).toBe(true);
  view.dispose();
  expect(mounts.every((m) => m.disposed)).toBe(true);
});

test("workspace hands off the same active chat, reconciles filtered membership, and opens document notifications", async () => {
  const store = new MessagingStore({ reportError: e => { throw e; } });
  const context = { allowed_company_ids: [5] };
  const direct = { id: 1, name: "Ada", channel_type: "direct", owner: false, latest: 80, seen: 0, unread: 1, bookmarks: 1, date: null, sender: "Ada", preview: "Hello" };
  const call = vi.fn(async (_: string, method: string, args: Record<string, unknown>) => {
    expect(args["context"]).toEqual(context);
    if (method === "conversation_search") return { items: args["query"] ? [] : [direct], next: null };
    if (method === "conversation_summary") return direct;
    if (method === "messaging_notifications") return [{ message: 10, model: "sale.order", res_id: 44, subject: "Order ready", unread: false }];
    throw new Error(method);
  });
  const openRecord = vi.fn();
  const view = renderDiscuss({ store, rpc: { call } as unknown as RpcClient, uid: 5, userName: "Admin", context, openRecord });
  const host = document.createElement("div");
  document.body.append(view.node, host); find(view.node, "Discuss").click();
  await vi.waitFor(() => expect(view.node.querySelector('[data-id="1"]')).toBeTruthy());
  view.node.querySelector<HTMLButtonElement>('[data-id="1"]')!.click();
  const original = mounts[0]!;
  const exit = view.openWorkspace(host);
  expect(host.querySelector(".discuss-workspace")).toBeTruthy();
  expect(host.querySelectorAll('[title="Invite people"]')).toHaveLength(0);
  const input = host.querySelector<HTMLInputElement>('[aria-label="Search conversations"]')!;
  input.value = "Nobody"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(host.textContent).toContain("No conversations found."));
  expect(host.querySelector(".discuss-identity h3")?.textContent).toBe("Ada");
  expect(original.disposed).toBe(false);
  find(host, "Notifications").click();
  await vi.waitFor(() => expect(host.querySelectorAll('.discuss-row')).toHaveLength(1));
  host.querySelector<HTMLButtonElement>(".discuss-row")!.click();
  expect(openRecord).toHaveBeenCalledWith("sale.order", 44);
  exit(); find(view.node, "Discuss").click();
  expect(view.node.querySelector(".discuss-identity h3")?.textContent).toBe("Ada");
  expect(mounts).toEqual([original]); view.dispose();
});

test("the notification shortcut focuses the inbox and acknowledgement updates the live count", async () => {
  vi.stubGlobal("WebSocket", Socket);
  vi.stubGlobal("BroadcastChannel", Bus);
  const context = { allowed_company_ids: [5] };
  let unread = true;
  const call = vi.fn(async (_: string, method: string, args: Record<string, unknown>) => {
    expect(args["context"]).toEqual(context);
    if (method === "messaging_notification_count") return Number(unread);
    if (method === "messaging_notifications") return [{ message: 10, model: "sale.order", res_id: 44, subject: "Order ready", unread }];
    if (method === "messaging_notification_read") {
      expect(args["message_id"]).toBe(10);
      unread = false;
      return null;
    }
    throw new Error(method);
  });
  const view = renderDiscuss({
    store: new MessagingStore({ reportError: e => { throw e; } }),
    rpc: { call } as unknown as RpcClient, uid: 5, userName: "Admin", context,
    policy: { company: 5, csrf: "test", retry: [1, 4] },
  });
  document.body.append(view.node);
  const changed = vi.fn();
  const unsubscribe = view.notifications.subscribe(changed);
  expect(Socket.all).toHaveLength(1);
  Socket.all[0]!.replay();
  await vi.waitFor(() => expect(changed).toHaveBeenCalledOnce());
  expect(await view.notifications.count()).toBe(1);
  view.notifications.open();
  const nav = find(view.node, "Notifications");
  expect(nav.getAttribute("aria-pressed")).toBe("true");
  expect(document.activeElement).toBe(nav);
  await vi.waitFor(() => find(view.node, "Mark Order ready as read"));
  expect(call.mock.calls.some(([, method]) => method === "messaging_notification_read")).toBe(false);
  find(view.node, "Mark Order ready as read").click();
  await vi.waitFor(() => expect(changed).toHaveBeenCalledTimes(2));
  expect(await view.notifications.count()).toBe(0);
  await vi.waitFor(() => expect(view.node.querySelector('[data-key="readn10"]')).toBeNull());
  unsubscribe();
  view.dispose();
});

test("opening Notifications cancels a saved conversation that is still loading", async () => {
  let finish!: (value: unknown) => void;
  const summary = new Promise(resolve => { finish = resolve; });
  const call = vi.fn(async (_: string, method: string) => {
    if (method === "conversation_search") return { items: [], next: null };
    if (method === "conversation_saved") return { items: [{ id: 5, channel_id: 7, channel_name: "Purchasing", preview: "Order" }], next: null };
    if (method === "conversation_summary") return await summary;
    if (method === "messaging_notifications") return [];
    throw new Error(method);
  });
  const view = renderDiscuss({ store: new MessagingStore({ reportError: e => { throw e; } }), rpc: { call } as unknown as RpcClient, uid: 5, userName: "A" });
  document.body.append(view.node);
  find(view.node, "Discuss").click();
  find(view.node, "Saved").click();
  await vi.waitFor(() => expect(view.node.querySelector('[data-key="s5"]')).toBeTruthy());
  view.node.querySelector<HTMLButtonElement>('[data-key="s5"]')!.click();
  expect(call.mock.calls.some(([, method]) => method === "conversation_summary")).toBe(true);
  view.notifications.open();
  await vi.waitFor(() => expect(view.node.textContent).toContain("No notifications."));
  finish({ id: 7, name: "Purchasing", channel_type: "group", owner: true, unread: 0, bookmarks: 1, seen: 0, latest: 5, preview: "Order", sender: null, date: null });
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(view.node.querySelector(".discuss-panel")?.classList.contains("has-thread")).toBe(false);
  expect(mounts).toHaveLength(0);
  view.dispose();
});

test("a notification refresh preserves an in-flight acknowledgement and keyboard focus", async () => {
  vi.stubGlobal("WebSocket", Socket);
  let acknowledge!: () => void;
  const ack = new Promise<void>(resolve => { acknowledge = resolve; });
  let title = "Order", first = true;
  const call = vi.fn(async (_: string, method: string) => {
    if (method === "messaging_notifications") return [{ message: first ? 10 : 11, model: null, res_id: null, subject: title, unread: true }];
    if (method === "messaging_notification_read") { await ack; first = false; return null; }
    throw new Error(method);
  });
  const view = renderDiscuss({ store: new MessagingStore({ reportError: e => { throw e; } }), rpc: { call } as unknown as RpcClient,
    uid: 5, userName: "A", policy: { company: 5, csrf: "test", retry: [1, 4] } });
  document.body.append(view.node);
  view.notifications.open();
  await vi.waitFor(() => find(view.node, "Mark Order as read"));
  const mark = find(view.node, "Mark Order as read");
  mark.focus();
  mark.click();
  title = "Order updated";
  Socket.all[0]!.replay();
  await vi.waitFor(() => find(view.node, "Mark Order updated as read"));
  const refreshed = find(view.node, "Mark Order updated as read");
  refreshed.click();
  expect(call.mock.calls.filter(([, method]) => method === "messaging_notification_read")).toHaveLength(1);
  expect(document.activeElement).toBe(refreshed);
  acknowledge();
  await vi.waitFor(() => expect(view.node.querySelector('[data-key="n11"]')).toBeTruthy());
  expect(document.activeElement).toBe(view.node.querySelector('[data-key="n11"]'));
  view.dispose();
});

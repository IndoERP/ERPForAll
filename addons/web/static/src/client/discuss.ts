/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { MessagingStore, type LocalModel } from "@erpforall/mail-store";
import { forgetDraft } from "@erpforall/mail-thread-view";
import { Chatter } from "./chatter.js";
import { watchChatterThread } from "./chatter-realtime.js";
import { ClientError, type RpcClient } from "./transport.js";

interface Channel {
  id: number;
  name: string;
  unread: number;
  bookmarks: number;
  latest: number | null;
  seen: number;
  owner: boolean;
}
interface Options {
  rpc: RpcClient;
  store: MessagingStore;
  uid: number;
  userName: string;
  policy: { company: number; csrf: string; retry: readonly [number, number] };
}
const summaries = new WeakMap<MessagingStore, LocalModel>();
function records(store: MessagingStore): LocalModel {
  const existing = summaries.get(store);
  if (existing) return existing;
  const model = store.defineLocalModel("DiscussSummary", ["channel"]);
  summaries.set(store, model);
  return model;
}

function decode(value: unknown): Channel[] {
  if (!Array.isArray(value) || value.length > 50)
    throw new ClientError("server", "Invalid conversation page.");
  let previous = 0;
  for (const row of value) {
    if (
      !row ||
      !Number.isSafeInteger(row.id) ||
      row.id <= previous ||
      typeof row.name !== "string" ||
      typeof row.owner !== "boolean" ||
      ![row.unread, row.bookmarks].every((n) => Number.isSafeInteger(n) && n >= 0 && n <= 1000) ||
      !Number.isSafeInteger(row.seen) ||
      row.seen < 0 ||
      (row.latest !== null && (!Number.isSafeInteger(row.latest) || row.latest < 1))
    ) {
      throw new ClientError("server", "Invalid conversation summary.");
    }
    previous = row.id;
  }
  return value as Channel[];
}

const button = (label: string, click: () => void) => {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.addEventListener("click", click);
  return b;
};
const count = (value: number) => (value === 1000 ? "999+" : String(value));

/** Each tab fetches with its own current session. The browser channel only carries a wake-up. */
export function renderDiscuss(options: Options): { node: HTMLElement; dispose(): void } {
  const { rpc, store, uid, policy } = options;
  const rows = records(store);
  const node = document.createElement("div");
  const panel = document.createElement("section");
  panel.className = "discuss-panel";
  panel.setAttribute("aria-label", "Discuss");
  panel.hidden = true;
  const status = document.createElement("p");
  status.setAttribute("role", "status");
  const list = document.createElement("div");
  const controls = document.createElement("div");
  const host = document.createElement("div");
  const inbox = document.createElement("div");
  inbox.setAttribute("aria-label", "Notifications");
  let inboxOpen = false;
  const title = document.createElement("h2");
  title.textContent = "Discuss";
  const toolbar = document.createElement("div");
  toolbar.className = "discuss-tools";
  const tabs = document.createElement("div");
  tabs.setAttribute("aria-label", "Open conversations");
  const current = document.createElement("h3");
  const dock = document.createElement("div");
  dock.className = "discuss-dock";
  const windows = new Map<number, { chatter: Chatter; element: HTMLElement; latest: number | null }>();
  function closeWindow(id: number) {
    const w = windows.get(id);
    w?.chatter.dispose();
    w?.element.remove();
    windows.delete(id);
  }
  function openWindow(c: Channel) {
    const existing = windows.get(c.id);
    if (existing) {
      existing.element.focus();
      return;
    }
    if (windows.size >= 3) {
      status.textContent = "Close a chat window before opening another.";
      return;
    }
    const element = document.createElement("section");
    element.className = "discuss-window";
    element.tabIndex = -1;
    element.setAttribute("aria-label", `Chat with ${c.name}`);
    const title = document.createElement("h3");
    title.textContent = c.name;
    const content = document.createElement("div");
    element.append(
      title,
      button("Close chat", () => closeWindow(c.id)),
      content,
    );
    dock.append(element);
    windows.set(c.id, {
      element,
      latest: c.latest,
      chatter: new Chatter({
        host: content,
        model: "discuss.channel",
        recordId: c.id,
        rpc,
        store,
        uid,
        userName: options.userName,
        reloadOnPost: true,
        reloadForm: invalidate,
      }),
    });
    element.focus();
  }
  let open = false,
    dead = false,
    version = 0,
    after = 0,
    selected = 0,
    activeLatest: number | null = null;
  let pending = false,
    again = false;
  const trail = [0];
  const chats = new Set<number>();
  let chatter: Chatter | undefined, stopSocket: (() => void) | undefined;
  const bus =
    typeof BroadcastChannel === "function"
      ? new BroadcastChannel(`erp.discuss.${uid}.${policy.company}`)
      : undefined;
  const all = () => rows.ids().map((id) => rows.get(id, "channel") as Channel);
  const channel = (id: number) => rows.get(String(id), "channel") as Channel | undefined;
  function clearChat() {
    chatter?.dispose();
    chatter = undefined;
    activeLatest = null;
    selected = 0;
    current.textContent = "";
    host.replaceChildren();
    controls.replaceChildren();
  }
  function clear(revoked = false) {
    for (const id of windows.keys()) closeWindow(id);
    clearChat();
    chats.clear();
    tabs.replaceChildren();
    list.replaceChildren();
    inbox.replaceChildren();
    for (const id of rows.ids()) {
      const ref = { model: "discuss.channel", id: Number(id) };
      if (revoked) {
        forgetDraft(store, ref);
        store.forgetThread(ref);
        rows.delete(id);
      } else store.windows.drop(ref);
    }
    store.collect();
  }
  function error(e: unknown) {
    if (dead) return;
    version++;
    clear(e instanceof ClientError && (e.kind === "permission" || e.kind === "session"));
    status.textContent = e instanceof Error ? e.message : "Discuss could not be loaded.";
  }
  async function refresh() {
    if (dead || !open) return;
    if (pending) {
      again = true;
      return;
    }
    pending = true;
    try {
      do {
        again = false;
        const generation = version;
        const [page, notices] = await Promise.all([
          rpc.call("discuss.channel", "conversation_list", { after }).then(decode),
          inboxOpen ? rpc.call("discuss.channel", "messaging_inbox", {}) : Promise.resolve(null),
        ]);
        if (dead || !open || generation !== version) continue;
        inbox.replaceChildren();
        if (inboxOpen && notices !== null) {
          if (!Array.isArray(notices) || notices.length > 50)
            throw new ClientError("server", "Invalid notification list.");
          const unique = new Set<number>();
          for (const notice of notices) {
            if (!Number.isSafeInteger(notice.message) || !["inbox", "push"].includes(notice.transport))
              throw new ClientError("server", "Invalid message notification.");
            if (unique.has(notice.message)) continue;
            unique.add(notice.message);
            const item = document.createElement("p");
            item.textContent = `New message #${notice.message}`;
            inbox.append(item);
          }
          if (!unique.size) inbox.textContent = "No notifications.";
        }
        const ids = new Set(page.map((c) => c.id));
        for (const id of rows.ids())
          if (!ids.has(Number(id))) {
            if (selected === Number(id)) clearChat();
            closeWindow(Number(id));
            chats.delete(Number(id));
            const ref = { model: "discuss.channel", id: Number(id) };
            forgetDraft(store, ref);
            store.forgetThread(ref);
            rows.delete(id);
          }
        for (const c of page) {
          const id = String(c.id);
          if (rows.has(id)) rows.set(id, { channel: c });
          else rows.create(id, { channel: c });
        }
        status.textContent = page.length ? "" : "No conversations on this page.";
        draw();
        const latest = channel(selected)?.latest ?? null;
        const updates: Promise<void>[] = [];
        if (chatter && latest !== activeLatest) {
          activeLatest = latest;
          updates.push(chatter.refresh());
        }
        for (const [id, w] of windows) {
          const now = channel(id)?.latest ?? null;
          if (now !== w.latest) {
            w.latest = now;
            updates.push(w.chatter.refresh());
          }
        }
        await Promise.all(updates);
      } while (again && open && !dead);
    } catch (e) {
      error(e);
    } finally {
      pending = false;
    }
  }
  function invalidate() {
    bus?.postMessage("refresh");
    return refresh();
  }
  if (bus)
    bus.onmessage = (e) => {
      if (e.data === "refresh") void refresh();
    };
  async function act(method: string, args: Record<string, unknown>) {
    try {
      await rpc.call("discuss.channel", method, args);
      await invalidate();
    } catch (e) {
      error(e);
    }
  }
  function openChannel(id: number) {
    const c = channel(id);
    if (!c) return;
    clearChat();
    selected = id;
    activeLatest = c.latest;
    chats.add(id);
    chatter = new Chatter({
      host,
      model: "discuss.channel",
      recordId: id,
      rpc,
      store,
      uid,
      userName: options.userName,
      reloadOnPost: true,
      reloadForm: invalidate,
    });
    draw();
  }
  function draw() {
    list.replaceChildren();
    tabs.replaceChildren();
    controls.replaceChildren();
    for (const c of all()) {
      list.append(
        button(`${c.name} · ${count(c.unread)} unread · ${count(c.bookmarks)} saved`, () =>
          openChannel(c.id),
        ),
      );
      if (chats.has(c.id)) {
        const tab = button(c.name, () => openChannel(c.id));
        tab.setAttribute("aria-pressed", String(c.id === selected));
        tabs.append(tab);
      }
    }
    const c = channel(selected);
    if (c) {
      current.textContent = c.name;
      controls.append(button("Open chat window", () => openWindow(c)));
      if (c.latest !== null)
        for (const [label, method, extra] of [
          ["Mark read", "conversation_seen", {}],
          ["Save latest", "conversation_bookmark", { saved: true }],
          ["Unsave latest", "conversation_bookmark", { saved: false }],
        ] as const)
          controls.append(
            button(label, () => void act(method, { ids: [c.id], message_id: c.latest, ...extra })),
          );
      if (c.latest !== null) {
        const receipt = document.createElement("output");
        controls.append(
          button("Read receipts", () => {
            void rpc
              .call("discuss.channel", "conversation_receipts", { ids: [c.id] })
              .then((result) => {
                if (!receipt.isConnected || !open || selected !== c.id) return;
                if (
                  !Array.isArray(result) ||
                  result.length > 100 ||
                  result.some((r) => !Number.isSafeInteger(r?.seen) || r.seen < 0)
                ) {
                  throw new ClientError("server", "Invalid read receipts.");
                }
                const readers = result.filter((r) => r.seen >= c.latest!).length;
                receipt.textContent = `${readers} of ${result.length} participants have marked the latest message as read.`;
              })
              .catch(error);
          }),
          receipt,
        );
      }
      if (c.owner) {
        const user = document.createElement("input");
        user.type = "number";
        user.min = "1";
        user.setAttribute("aria-label", "User ID to invite");
        controls.append(
          user,
          button("Invite", () => {
            if (user.valueAsNumber > 0)
              void act("conversation_invite", { ids: [c.id], user_id: user.valueAsNumber });
          }),
        );
      }
      controls.append(button("Leave conversation", () => void act("conversation_leave", { ids: [c.id] })));
    }
    prev.disabled = trail.length === 1;
    next.disabled = rows.ids().length < 50;
  }
  function watchPage() {
    stopSocket?.();
    if (typeof WebSocket === "function")
      stopSocket = watchChatterThread(
        { model: "discuss.channel.member", id: after + 1 },
        policy,
        refresh,
        error,
      );
    void refresh();
  }
  function close() {
    open = false;
    version++;
    stopSocket?.();
    stopSocket = undefined;
    clear();
    panel.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
  }
  const trigger = button("Discuss", () => {
    if (open) {
      close();
      return;
    }
    open = true;
    panel.hidden = false;
    trigger.setAttribute("aria-expanded", "true");
    watchPage();
  });
  trigger.setAttribute("aria-expanded", "false");
  const prev = button("Previous", () => {
    if (trail.length > 1) {
      trail.pop();
      after = trail.at(-1)!;
      version++;
      watchPage();
    }
  });
  const next = button("Next", () => {
    const last = all().at(-1);
    if (last) {
      after = last.id;
      trail.push(after);
      version++;
      watchPage();
    }
  });
  const name = document.createElement("input");
  name.setAttribute("aria-label", "Conversation name");
  name.maxLength = 120;
  toolbar.append(
    button("Notifications", () => {
      inboxOpen = !inboxOpen;
      version++;
      void refresh();
    }),
    name,
    button("Create conversation", () => {
      if (name.value.trim()) {
        void act("conversation_create", { name: name.value.trim() });
        name.value = "";
      }
    }),
    button("Close", close),
  );
  panel.append(title, toolbar, inbox, list, prev, next, tabs, current, controls, host, status);
  node.append(trigger, panel, dock);
  return {
    node,
    dispose() {
      dead = true;
      close();
      clear(true);
      bus?.close();
      node.remove();
    },
  };
}

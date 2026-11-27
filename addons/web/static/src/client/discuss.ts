/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { MessagingStore, type LocalModel } from "@erpforall/mail-store";
import { forgetDraft } from "@erpforall/mail-thread-view";
import { Chatter, type ChatterOptions } from "./chatter.js";
import { watchChatterThread } from "./chatter-realtime.js";
import { button, el } from "./dom.js";
import { peoplePicker, type DiscussCall } from "./discuss-picker.js";
import { icon, type IconName } from "./icons.js";
import type { DiscussNotifications } from "./systray.js";
import { ClientError, type RpcClient } from "./transport.js";

interface Channel {
  id: number; name: string; unread: number; bookmarks: number; seen: number;
  latest: number | null; owner: boolean; channel_type: "direct" | "group";
  preview: string; date: string | null; sender: string | null;
}
interface Options {
  rpc: RpcClient; store: MessagingStore; uid: number; userName: string;
  policy?: ChatterOptions["threadSocket"]; context?: Record<string, unknown>;
  openWorkspace?: () => void; openRecord?: (model: string, id: number) => void;
}
export interface DiscussControl {
  node: HTMLElement;
  notifications: DiscussNotifications;
  openWorkspace(host: HTMLElement): () => void;
  dispose(): void;
}
interface Entry {
  key: string; name: string; detail: string; initials?: string; unread?: number; date?: string | null;
  open(): void; remove?(): void; markRead?(): Promise<void>;
}
interface Notice { message: number; subject: string | null; model: string | null; res_id: number | null; unread: boolean }
type Section = "all" | "unread" | "saved" | "inbox";
const models = new WeakMap<MessagingStore, LocalModel>();
const positive = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value > 0;
function notification(value: unknown): Notice {
  const row = value as Partial<Notice> | null;
  if (!row || !positive(row.message) || typeof row.unread !== "boolean"
      || (row.subject !== null && typeof row.subject !== "string")
      || (row.model !== null && typeof row.model !== "string")
      || (row.res_id !== null && !positive(row.res_id))
      || ((row.model === null) !== (row.res_id === null))) throw new Error("Invalid notification.");
  return row as Notice;
}

function channel(value: unknown): Channel {
  if (typeof value !== "object" || !value) throw new Error("Invalid conversation summary.");
  const c = value as Channel;
  if (!positive(c.id) || typeof c.name !== "string" || !["direct", "group"].includes(c.channel_type) ||
    ![c.seen, c.unread, c.bookmarks].every(n => Number.isSafeInteger(n) && n >= 0) ||
    (c.latest !== null && !positive(c.latest)) || typeof c.owner !== "boolean" || typeof c.preview !== "string" ||
    [c.sender, c.date].some(s => s !== null && typeof s !== "string")) throw new Error("Invalid conversation summary.");
  return c;
}
function page(value: unknown): { items: unknown[]; next: unknown } {
  if (!value || typeof value !== "object" || !("items" in value) || !Array.isArray(value.items) || value.items.length > 50 || !("next" in value)) {
    throw new Error("Invalid Discuss page.");
  }
  return { items: value.items, next: value.next };
}
function iconButton(label: string, name: IconName, action: () => void) {
  const b = button("", action, "discuss-icon");
  b.append(icon(name)); b.title = label; b.setAttribute("aria-label", label);
  return b;
}
const initials = (name: string) => name.trim().split(/\s+/).slice(0, 2).map(part => Array.from(part)[0]).join("").toLocaleUpperCase();
function timeLabel(value: string): string {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return "";
  return date.toDateString() === new Date().toDateString() ? date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" }) :
    date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function renderDiscuss(options: Options): DiscussControl {
  const { rpc, store, uid, policy, context } = options;
  const rows = models.get(store) ?? store.defineLocalModel("DiscussSummary", ["channel"]);
  models.set(store, rows);
  const call: DiscussCall = (method, args, write = false) => rpc.call("discuss.channel", method,
    { ...args, ...(context ? { context } : {}) }, { write, background: !write });
  const node = el("div", undefined, "discuss-control");
  const panel = el("section", undefined, "discuss-panel");
  panel.setAttribute("aria-label", "Discuss"); panel.hidden = true;
  const side = el("aside", undefined, "discuss-sidebar");
  const top = el("header", undefined, "discuss-sidebar-head");
  const newMessage = iconButton("New message", "plus", () => pick(null));
  const expand = iconButton("Open workspace", "external-link", () => options.openWorkspace?.());
  const close = iconButton("Close", "x", closeQuick);
  top.append(el("h2", "Discuss"), newMessage);
  if (options.openWorkspace) top.append(expand);
  top.append(close);
  const search = el("input");
  search.type = "search"; search.maxLength = 120; search.placeholder = "Search conversations";
  search.setAttribute("aria-label", search.placeholder);
  const searchRow = el("div", undefined, "discuss-search");
  searchRow.append(icon("search"), search);
  const nav = el("nav", undefined, "discuss-nav");
  nav.setAttribute("aria-label", "Discuss sections");
  const list = el("div", undefined, "discuss-list");
  list.setAttribute("aria-label", "Conversations");
  const pages = el("div", undefined, "discuss-pages");
  const backPage = iconButton("Previous", "chevron-left", () => { if (history.length > 1) { history.pop(); void refresh(); } });
  const nextPage = iconButton("Next", "chevron-right", () => { if (next !== null) { history.push(next); void refresh(); } });
  pages.append(backPage, nextPage);
  const thread = el("section", undefined, "discuss-main");
  const head = el("header", undefined, "discuss-thread-head");
  const back = iconButton("Back to conversations", "arrow-left", () => {
    panel.classList.remove("has-thread"); searchRow.hidden ? nav.querySelector("button")?.focus() : search.focus();
  });
  back.classList.add("discuss-back");
  const identity = el("div", undefined, "discuss-identity");
  const name = el("h3"), kind = el("span");
  identity.append(name, kind);
  const actions = el("div", undefined, "discuss-tools");
  head.append(back, identity, actions);
  const body = el("div", undefined, "discuss-thread-body");
  const empty = el("div", undefined, "discuss-empty");
  empty.append(icon("messages-square"), el("h3", "Select a conversation"), button("New message", () => pick(null)));
  const picker = el("div", undefined, "discuss-picker-slot");
  picker.hidden = true;
  const status = el("div", undefined, "discuss-status");
  status.setAttribute("role", "status");
  thread.append(head, body, empty, picker);
  side.append(top, searchRow, nav, list, pages);
  panel.append(side, thread, status);
  const dock = el("div", undefined, "discuss-dock");
  const trigger = button("Discuss", () => {
    if (workspace) { search.focus(); return; }
    if (panel.hidden) show(); else closeQuick();
  });
  trigger.setAttribute("aria-expanded", "false"); trigger.setAttribute("aria-label", "Discuss");
  node.append(trigger, panel, dock);
  const visited = new Set<number>();
  const windows = new Map<number, { element: HTMLElement; chatter: Chatter; latest: number | null }>();
  let selected: Channel | null = null, chatter: Chatter | undefined, section: Section = "all";
  const subscriptions = new Map<number, () => void>();
  const notificationListeners = new Set<() => void>();
  let workspace: HTMLElement | null = null, dead = false, generation = 0, selection = 0;
  let history: (string | number | null)[] = [null], next: string | number | null = null;
  let endPicker: (() => void) | undefined, endSocket: (() => void) | undefined;
  let searchTimer: ReturnType<typeof setTimeout>;
  const allowed = context?.["allowed_company_ids"];
  const company = policy?.company ?? (Array.isArray(allowed) && positive(allowed[0]) ? allowed[0] : null);
  const bus = company !== null && typeof BroadcastChannel === "function" ? new BroadcastChannel(`erp.discuss.${uid}.${company}`) : undefined;
  if (bus) bus.onmessage = event => {
    if (event.data === "notifications") notificationsChanged();
    else if (event.data === "refresh") void refresh();
  };
  const links = new Map<Section, HTMLButtonElement>();
  for (const [id, label] of [["all","Conversations"],["unread","Unread"],["saved","Saved"],["inbox","Notifications"]] as const) {
    const b = button(label, () => selectSection(id));
    links.set(id, b); nav.append(b);
  }
  search.addEventListener("input", () => {
    ++generation; history = [null]; clearTimeout(searchTimer);
    searchTimer = setTimeout(() => void refresh(), 150);
  });
  panel.addEventListener("keydown", e => {
    if (e.key === "Escape" && !e.defaultPrevented && !workspace) { closeQuick(); trigger.focus(); }
  });
  function selectSection(id: Section, load = true) {
    section = id; history = [null]; list.replaceChildren(); next = null;
    if (id === "inbox") {
      closePicker(false);
      panel.classList.remove("has-thread");
    }
    if (load) void refresh();
  }
  function notificationsChanged() {
    for (const listener of notificationListeners) listener();
    if (section === "inbox") void refresh();
  }
  function persist(c: Channel) {
    const id = String(c.id);
    if (rows.has(id)) rows.set(id, { channel: c }); else rows.create(id, { channel: c });
  }
  function closeWindow(id: number) {
    const win = windows.get(id);
    if (win) { win.chatter.dispose(); win.element.remove(); windows.delete(id); }
    syncWatch();
  }
  function dropSelected() {
    selected = null; chatter?.dispose(); chatter = undefined; body.replaceChildren();
    head.hidden = true; empty.hidden = false; panel.classList.remove("has-thread");
  }
  function forget(id: number) {
    if (selected?.id === id) dropSelected();
    closeWindow(id);
    const ref = { model: "discuss.channel", id };
    forgetDraft(store, ref); store.forgetThread(ref); visited.delete(id);
    if (rows.has(String(id))) rows.delete(String(id));
  }
  function clearPrivate() {
    ++generation; ++selection; closePicker(false);
    for (const id of new Set([...visited, ...rows.ids().map(Number)])) forget(id);
    list.replaceChildren(); pages.hidden = true; store.collect();
  }
  function fail(e: unknown) {
    if (dead) return;
    if (e instanceof ClientError && (e.kind === "permission" || e.kind === "session")) clearPrivate();
    status.textContent = e instanceof Error ? e.message : "Discuss could not be loaded.";
  }
  function syncWatch() {
    const needed = !dead && (!panel.hidden || windows.size > 0);
    const keepSummary = !dead && (needed || notificationListeners.size > 0);
    if (!keepSummary) { endSocket?.(); endSocket = undefined; }
    const active = needed ? new Set(windows.keys()) : new Set<number>();
    if (needed && !panel.hidden && selected) active.add(selected.id);
    for (const [id, dispose] of subscriptions) {
      if (!active.has(id)) { dispose(); subscriptions.delete(id); }
    }
    if (!keepSummary || !policy || typeof WebSocket !== "function") return;
    endSocket ??= watchChatterThread({ model: "discuss.channel.member", id: Number.MAX_SAFE_INTEGER }, policy, async () => {
      for (const listener of notificationListeners) listener();
      await refresh();
    }, error => {
      fail(error);
      for (const listener of notificationListeners) listener();
    });
    // An open conversation can fall outside the recent page. Recheck it independently on edits or loss of membership.
    for (const id of active) if (!subscriptions.has(id)) {
      subscriptions.set(id, watchChatterThread({ model: "discuss.channel", id }, policy, async () => {
        const mounted = new Set([selected?.id === id ? chatter : undefined, windows.get(id)?.chatter]);
        await Promise.all([...mounted].map(view => view?.refresh()));
      }, error => {
        if (error instanceof ClientError && error.kind === "permission") { forget(id); void refresh(); }
        else fail(error);
      }));
    }
  }
  function show() {
    panel.hidden = false; trigger.setAttribute("aria-expanded", "true"); syncWatch(); void refresh();
  }
  function closeQuick() {
    if (workspace) return;
    ++generation; ++selection; closePicker(false); panel.hidden = true; trigger.setAttribute("aria-expanded", "false"); syncWatch();
  }
  async function invalidate() { bus?.postMessage("refresh"); await refresh(); }
  async function write(method: string, args: Record<string, unknown>) {
    await call(method, args, true);
    if (!dead) await invalidate();
  }
  function closePicker(restoreFocus = true) {
    if (!endPicker) return;
    endPicker(); endPicker = undefined; picker.hidden = true; thread.classList.remove("picking");
    if (!selected) panel.classList.remove("has-thread");
    if (restoreFocus) newMessage.focus();
  }
  function pick(invite: number | null) {
    closePicker(false);
    picker.hidden = false; thread.classList.add("picking"); panel.classList.add("has-thread");
    endPicker = peoplePicker(picker, call, invite, async id => {
      closePicker(false); section = "all"; history = [null]; search.value = "";
      await openId(id); await invalidate();
    }, () => closePicker(), fail);
  }
  function mount(c: Channel, host: HTMLElement) {
    visited.add(c.id);
    return new Chatter({ host, model: "discuss.channel", recordId: c.id, rpc, uid, store, userName: options.userName,
      ...(context ? { context } : {}), reloadOnPost: true, reloadForm: invalidate,
      conversation: {
        markRead: message_id => write("conversation_seen", { ids: [c.id], message_id }),
        save: async message_id => { await write("conversation_bookmark", { ids: [c.id], message_id, saved: true }); return "Message saved."; },
        receipts: async message_id => {
          const result = await call("conversation_receipts", { ids: [c.id] });
          if (!Array.isArray(result) || result.length > 100 || result.some(r => !Number.isSafeInteger(r?.seen) || r.seen < 0)) throw new Error("Invalid read receipts.");
          return `${result.filter(r => r.seen >= message_id).length} of ${result.length} participants have marked this message as read.`;
        }
      }
    });
  }
  function openWindow(c: Channel) {
    const existing = windows.get(c.id);
    if (existing) { existing.element.focus(); return; }
    if (windows.size >= 3) { status.textContent = "Close a chat window before opening another."; return; }
    const element = el("section", undefined, "discuss-window"); element.tabIndex = -1;
    element.setAttribute("aria-label", `Chat with ${c.name}`);
    const bar = el("header"); bar.append(el("h3", c.name), iconButton("Close chat", "x", () => closeWindow(c.id)));
    const host = el("div", undefined, "discuss-thread-body"); element.append(bar, host); dock.append(element);
    windows.set(c.id, { element, chatter: mount(c, host), latest: c.latest }); element.focus();
  }
  function header(c: Channel) {
    name.textContent = c.name; kind.textContent = c.channel_type === "direct" ? "Direct message" : "Group conversation";
    const key = `${c.id}:${c.channel_type}:${c.owner}`;
    if (actions.dataset["key"] === key) return;
    actions.dataset["key"] = key; actions.replaceChildren();
    actions.append(iconButton("Open chat window", "external-link", () => openWindow(selected!)));
    if (c.channel_type === "group") {
      if (c.owner) actions.append(iconButton("Invite people", "users", () => pick(c.id)));
      actions.append(iconButton("Leave conversation", "log-out", () => { void write("conversation_leave", { ids: [c.id] }).catch(fail); }));
    }
  }
  function select(c: Channel, target?: number) {
    if (dead) return;
    ++selection; closePicker(false); panel.classList.add("has-thread");
    if (c.id !== selected?.id) {
      chatter?.dispose(); body.replaceChildren(); chatter = mount(c, body);
    }
    selected = c; persist(c); head.hidden = false; empty.hidden = true; header(c); syncWatch();
    if (list.contains(document.activeElement) && typeof matchMedia === "function" && matchMedia("(max-width: 700px)").matches) back.focus();
    for (const b of list.querySelectorAll("button[data-id]")) b.setAttribute("aria-current", String(b.getAttribute("data-id") === String(c.id)));
    if (target) void chatter?.focusMessage(target).catch(fail);
  }
  async function openId(id: number, message?: number) {
    const token = ++selection;
    try {
      const raw = await call("conversation_summary", { ids: [id] });
      if (dead || selection !== token) return;
      if (!raw) { forget(id); status.textContent = "This conversation is no longer available."; return; }
      select(channel(raw), message);
    } catch (error) { if (token === selection) fail(error); }
  }
  function renderList(entries: Entry[]) {
    const focus = document.activeElement instanceof HTMLElement && list.contains(document.activeElement) ? document.activeElement.dataset["key"] : null;
    list.replaceChildren();
    for (const e of entries) {
      const row = el("div", undefined, "discuss-entry");
      const b = button("", e.open, "discuss-row"); b.dataset["key"] = e.key;
      if (e.key.startsWith("c")) {
        b.dataset["id"] = e.key.slice(1); b.setAttribute("aria-current", String(b.dataset["id"] === String(selected?.id)));
      }
      if (e.initials !== undefined) { const badge = el("span", e.initials, "discuss-avatar"); badge.setAttribute("aria-hidden", "true"); b.append(badge); }
      const copy = el("span", undefined, "discuss-row-copy"); copy.append(el("strong", e.name), el("small", e.detail));
      b.append(copy);
      const meta = el("span", undefined, "discuss-meta");
      if (e.date) { const time = el("time", timeLabel(e.date)); time.dateTime = e.date; meta.append(time); }
      if (e.unread) { const badge = el("span", e.unread >= 1000 ? "999+" : String(e.unread), "discuss-unread"); badge.setAttribute("aria-label", `${e.unread} unread`); meta.append(badge); }
      b.append(meta); row.append(b);
      if (e.remove) { const remove = iconButton("Remove saved message", "x", e.remove); remove.dataset["key"] = "remove" + e.key; row.append(remove); }
      if (e.markRead) {
        const markRead = iconButton(`Mark ${e.name} as read`, "square-check-big", () => {
          markRead.disabled = true;
          void e.markRead!().catch(fail).finally(() => { markRead.disabled = false; });
        });
        markRead.dataset["key"] = "read" + e.key;
        row.append(markRead);
      }
      list.append(row);
    }
    if (!entries.length) list.append(el("p", section === "saved" ? "No saved messages." : section === "inbox" ? "No notifications." :
      section === "unread" ? "No unread conversations." : "No conversations found.", "discuss-list-empty"));
    if (focus) {
      const controls = Array.from(list.querySelectorAll<HTMLButtonElement>("button"));
      const same = controls.find(b => b.dataset["key"] === focus);
      const marked = focus.startsWith("readn") ? controls.find(b => b.dataset["key"] === focus.slice(4)) : undefined;
      (same ?? marked)?.focus({ preventScroll: true });
    }
  }
  async function refresh() {
    if (dead || (panel.hidden && !windows.size)) return;
    const token = ++generation, cursor = history.at(-1), found = new Map<number, Channel>();
    const sectionAtStart = section;
    for (const [key, link] of links) link.setAttribute("aria-pressed", String(key === section));
    searchRow.hidden = section === "inbox" || section === "saved";
    const isCurrent = () => !dead && token === generation;
    try {
      let entries: Entry[] = [], nextCursor: number | string | null = null;
      if (section === "all" || section === "unread") {
        const response = page(await call("conversation_search", { query: search.value.trim(), filter: section, ...(cursor ? { cursor } : {}) }));
        if (!isCurrent()) return;
        if (response.next !== null && typeof response.next !== "string") throw new Error("Invalid conversation cursor.");
        nextCursor = response.next;
        for (const raw of response.items) {
          const c = channel(raw); found.set(c.id, c);
          entries.push({ key: "c" + c.id, name: c.name, detail: c.preview ? [c.sender, c.preview].filter(Boolean).join(" · ") : "No messages yet",
            date: c.date, unread: c.unread, initials: initials(c.name), open: () => select(c) });
        }
      } else if (section === "saved") {
        const response = page(await call("conversation_saved", cursor ? { cursor } : {}));
        if (!isCurrent()) return;
        if (response.next !== null && !positive(response.next)) throw new Error("Invalid saved message cursor.");
        nextCursor = response.next;
        for (const raw of response.items) {
          const s = raw as { id: number; channel_id: number; channel_name: string; preview: string };
          if (!s || !positive(s.id) || !positive(s.channel_id) || typeof s.channel_name !== "string" || typeof s.preview !== "string") throw new Error("Invalid saved message.");
          entries.push({ key: "s" + s.id, name: s.channel_name, detail: s.preview, open: () => void openId(s.channel_id, s.id),
            remove: () => { void write("conversation_bookmark", { ids: [s.channel_id], message_id: s.id, saved: false }).catch(fail); } });
        }
      } else {
        const notices = await call("messaging_notifications", {});
        if (!isCurrent()) return;
        if (!Array.isArray(notices) || notices.length > 50) throw new Error("Invalid notifications.");
        const seen = new Set<number>();
        for (const raw of notices) {
          const n = notification(raw);
          if (seen.has(n.message)) continue;
          seen.add(n.message);
          entries.push({ key: "n" + n.message, name: n.subject || "New message", detail: n.model ?? "",
            ...(n.unread ? { unread: 1, markRead: async () => {
              await call("messaging_notification_read", { message_id: n.message }, true);
              if (!dead) {
                bus?.postMessage("notifications");
                notificationsChanged();
              }
            } } : {}),
            open: () => {
              if (n.model && n.res_id && options.openRecord) options.openRecord(n.model, n.res_id);
              else status.textContent = "This notification has no available document.";
            } });
        }
      }
      const activeIds = new Set(windows.keys());
      if (selected) activeIds.add(selected.id);
      const missing = [...activeIds].filter(id => !found.has(id));
      const summaries = await Promise.all(missing.map(id => call("conversation_summary", { ids: [id] })));
      if (!isCurrent()) return;
      missing.forEach((id, index) => {
        const raw = summaries[index];
        if (raw) found.set(id, channel(raw)); else forget(id);
      });
      for (const id of rows.ids()) if (!found.has(Number(id))) rows.delete(id);
      for (const c of found.values()) persist(c);
      next = nextCursor; nextPage.disabled = next === null; backPage.disabled = history.length === 1; pages.hidden = nextPage.disabled && backPage.disabled;
      status.textContent = ""; list.setAttribute("aria-label", links.get(sectionAtStart)!.textContent!); renderList(entries);
      const updates: Promise<void>[] = [];
      if (selected && found.has(selected.id)) {
        const c = found.get(selected.id)!;
        if (c.latest !== selected.latest && chatter) updates.push(chatter.refresh());
        selected = c; header(c);
      }
      for (const [id, win] of windows) {
        const c = found.get(id);
        if (c && c.latest !== win.latest) { win.latest = c.latest; updates.push(win.chatter.refresh()); }
      }
      await Promise.all(updates);
    } catch (e) { if (isCurrent()) fail(e); }
  }
  head.hidden = true; pages.hidden = true;
  return {
    node,
    notifications: {
      async count() {
        const raw = await call("messaging_notification_count", {});
        if (typeof raw !== "number" || !Number.isSafeInteger(raw) || raw < 0) throw new Error("Invalid notification count.");
        return raw;
      },
      open() {
        if (dead) return;
        selectSection("inbox", false);
        show();
        links.get("inbox")?.focus();
      },
      subscribe(listener) {
        if (dead) return () => {};
        notificationListeners.add(listener);
        syncWatch();
        return () => {
          notificationListeners.delete(listener);
          syncWatch();
        };
      },
    },
    openWorkspace(host) {
      workspace = host; host.append(panel); panel.classList.add("discuss-workspace"); expand.hidden = true; close.hidden = true; show();
      return () => {
        if (workspace !== host || dead) return;
        workspace = null; panel.classList.remove("discuss-workspace"); expand.hidden = false; close.hidden = false;
        node.insertBefore(panel, dock); closeQuick();
      };
    },
    dispose() {
      if (dead) return;
      dead = true; clearTimeout(searchTimer); syncWatch(); bus?.close(); notificationListeners.clear(); clearPrivate(); panel.remove(); node.remove();
    }
  };
}

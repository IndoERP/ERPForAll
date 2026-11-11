/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { effect, signal, untrack, type Signal } from "@erpforall/elang";
import type { Lease } from "@erpforall/mail-store";
import { checkBaseOptions, createContext, t, type ThreadViewBaseOptions } from "./context";
import type { MessageData } from "./contract";
import { Announcer, h, uniqueId } from "./dom";
import { ThreadViewError } from "./errors";
import { formatter, Message } from "./message";
import { discardPending, sendPending } from "./posting";
import type { ThreadViewContext } from "./registries";
import { pendingUids, uiModels } from "./state";

/** Options of a {@link Thread}. */
export interface ThreadOptions extends ThreadViewBaseOptions {
  /** `desc` shows the newest message first (the chatter, TH-01); `asc` shows it last (Discuss). Default `desc`. */
  readonly order?: "asc" | "desc";
  /** Default: on for `asc`, off for `desc`. */
  readonly squash?: boolean;
  /** The thread's name, for the subject rule of its messages. */
  readonly threadName?: () => string | undefined;
  /** The element that scrolls the thread; defaults to the thread's own element. */
  readonly scroller?: () => HTMLElement | null;
}

const THREAD_OPTION_KEYS = ["order", "squash", "threadName", "scroller"];
const SQUASH_WINDOW_MS = 5 * 60 * 1000;
const AT_END_PX = 30;
const MAX_REPORTED_AUTHORS = 64;

/** The scroll offset to add after older messages were added, so the reader's place does not move. */
export function anchorDelta(order: "asc" | "desc", heightBefore: number, heightAfter: number): number {
  return order === "asc" ? heightAfter - heightBefore : 0;
}

/** `YYYY-MM-DD` of an instant in a time zone (the `en-CA` format is ISO-shaped). */
function dayKey(date: Date, timeZone: string | undefined): string {
  return new Intl.DateTimeFormat("en-CA", { year: "numeric", month: "2-digit", day: "2-digit", ...(timeZone !== undefined ? { timeZone } : {}) }).format(date);
}

function previousDayKey(key: string): string {
  const [y, m, d] = key.split("-").map(Number) as [number, number, number];
  return new Date(Date.UTC(y, m - 1, d - 1)).toISOString().slice(0, 10);
}

/** Make `parent`'s children exactly `desired`, moving only the nodes that are out of place (so focus survives). */
function reconcileChildren(parent: Element, desired: readonly Node[]): void {
  const wanted = new Set<Node>(desired);
  for (const child of Array.from(parent.childNodes)) if (!wanted.has(child)) parent.removeChild(child);
  desired.forEach((node, index) => {
    const at = parent.childNodes[index] ?? null;
    if (at !== node) parent.insertBefore(node, at);
  });
}

/**
 * The messages of one thread: a feed of {@link Message} articles over the thread's store window, with a day label on the first
 * message of each day, unsent posts, a "Load older" control and empty, loading and error states.
 *
 * Patch points (typed `patch(Thread, …)`): {@link Thread.isSquashed}, {@link Thread.dayLabel}, {@link Thread.emptyLabel}.
 *
 * The rows are laid out by {@link Thread.layout}; moving to a variable-height virtual list replaces that method
 * .
 */
export class Thread {
  /** The thread's root element. */
  readonly element: HTMLElement;
  readonly #options: ThreadOptions;
  readonly #view: ThreadViewContext;
  readonly #order: "asc" | "desc";
  readonly #squash: boolean;
  readonly #feed: HTMLElement;
  readonly #status: HTMLElement;
  readonly #older: HTMLButtonElement;
  readonly #announcer: Announcer;
  readonly #rows = new Map<number, Message>();
  readonly #empty: HTMLElement;
  readonly #pendingRows = new Map<string, PendingRow>();
  readonly #lease: Lease;
  readonly #stops: (() => void)[] = [];
  readonly #loadingLatest: Signal<boolean> = signal(false);
  readonly #loadingOlder: Signal<boolean> = signal(false);
  readonly #failure: Signal<{ readonly what: "latest" | "older"; readonly text: string } | null> = signal<{ readonly what: "latest" | "older"; readonly text: string } | null>(null);
  readonly #reportedAuthors = new Set<number>();
  #ids: readonly number[] = [];
  #current: number | null = null;
  #disposed = false;
  #ready = false;
  #initError: { readonly error: unknown } | null = null;

  constructor(options: ThreadOptions) {
    checkBaseOptions(options, THREAD_OPTION_KEYS, "Thread");
    if (options.order !== undefined && options.order !== "asc" && options.order !== "desc") {
      throw new ThreadViewError("bad_option", `options.order must be "asc" or "desc", got ${JSON.stringify(options.order)}`);
    }
    this.#options = options;
    this.#order = options.order ?? "desc";
    this.#squash = options.squash ?? this.#order === "asc";
    const doc = options.document ?? document;
    this.#announcer = new Announcer(doc);
    this.#view = createContext(options, (text, urgency) => this.#announcer.say(text, urgency));
    const view = this.#view;
    this.#lease = options.store.hold(options.ref.model, options.ref.id);
    const feedLabel = t(view, "Messages");
    this.#feed = h(doc, "div", { class: "erp-mail-feed", role: "feed", "aria-label": feedLabel, "aria-busy": "false" });
    this.#status = h(doc, "div", { class: "erp-mail-thread-status" });
    this.#older = h(doc, "button", { type: "button", class: "erp-mail-load-older", hidden: true }, t(view, "Load older messages"));
    this.#older.addEventListener("click", () => void this.#loadOlder());
    this.#empty = h(doc, "p", { class: "erp-mail-empty", hidden: true });
    const parts: Node[] = this.#order === "asc"
      ? [this.#announcer.element, this.#status, this.#older, this.#feed, this.#empty]
      : [this.#announcer.element, this.#status, this.#empty, this.#feed, this.#older];
    this.element = h(doc, "div", { class: "erp-mail-thread", "data-order": this.#order, id: uniqueId("erp-mail-thread") }, ...parts);
    this.#feed.addEventListener("keydown", (event) => this.#onKeydown(event));
    this.#feed.addEventListener("focusin", (event) => this.#onFocusIn(event));
    try {
      this.#stops.push(effect(() => {
        try {
          this.#sync();
        } catch (error) {
          // A failure of the first layout is the constructor's to throw; a later one is reported (the effect would
          // otherwise hand it to the global report channel, away from this thread's own).
          if (this.#ready) this.#view.reportError(error);
          else this.#initError = { error };
        }
      }));
      if (this.#initError !== null) throw this.#initError.error;
      this.#ready = true;
    } catch (error) {
      // The first layout failed: nothing may stay held for a thread that never mounted.
      for (const stop of this.#stops) stop();
      for (const row of this.#rows.values()) row.dispose();
      this.#lease.release();
      throw error;
    }
    if (untrack(() => options.store.windows.get(options.ref)) === undefined) void this.#loadLatest();
  }

  // ---- patch points

  isSquashed(message: MessageData, previous: MessageData | undefined): boolean {
    if (previous === undefined || previous.messageType === "notification") return false;
    if (message.authorId === null || message.authorId !== previous.authorId) return false;
    if (message.messageType !== previous.messageType) return false;
    if (message.isInternal || previous.isInternal) return false;
    return message.date.getTime() - previous.date.getTime() < SQUASH_WINDOW_MS;
  }

  /** The label of a day separator: "Today", "Yesterday" or the date. */
  dayLabel(date: Date, now: Date): string {
    const { locale, timeZone } = this.#view.services;
    const key = dayKey(date, timeZone);
    const today = dayKey(now, timeZone);
    if (key === today) return t(this.#view, "Today");
    if (key === previousDayKey(today)) return t(this.#view, "Yesterday");
    return formatter(locale, timeZone, "date").format(date);
  }

  /** The text of the empty thread. */
  emptyLabel(): string {
    return t(this.#view, "No messages yet");
  }

  // ---- public

  /** The thread's context (the shared state of its actions, renderers and composer). */
  get context(): ThreadViewContext {
    return this.#view;
  }

  /** The message ids in the order shown. */
  displayedIds(): number[] {
    return this.#order === "desc" ? [...this.#ids].reverse() : [...this.#ids];
  }

  /** The row of a message, if it is in the window. */
  message(id: number): Message | undefined {
    return this.#rows.get(id);
  }

  /**
   * Move focus to a message's article.
   *
   * @param id - a message in the window
   * @throws {@link ThreadViewError} `bad_option` for a message not in the window, `disposed` after `dispose()`
   */
  focusMessage(id: number): void {
    this.#alive();
    const row = this.#rows.get(id);
    if (row === undefined) throw new ThreadViewError("bad_option", `message ${id} is not in the window of ${this.#view.ref.model}#${this.#view.ref.id}`);
    this.#setCurrent(id);
    row.element.focus();
  }

  /** Stop following the store, release the lease and remove the element. Idempotent. */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    for (const stop of this.#stops) stop();
    for (const row of this.#rows.values()) row.dispose();
    for (const row of this.#pendingRows.values()) row.dispose();
    this.#rows.clear();
    this.#pendingRows.clear();
    this.#lease.release();
    this.element.remove();
  }

  // ---- layout

  /**
   * Bring the rows in line with a window: create, remove and re-flag the messages, then lay out messages
   * (with their day labels) and unsent posts. This is the seam for a virtual list.
   */
  protected layout(ids: readonly number[]): void {
    const previousIds = this.#ids;
    const scroller = this.#options.scroller?.() ?? this.element;
    const before = { height: scroller.scrollHeight, top: scroller.scrollTop, atEnd: this.#atEnd(scroller) };
    const keep = new Set(ids);
    const hadFocus = this.#feed.contains(this.element.ownerDocument.activeElement);
    const removedCurrent = this.#current !== null && !keep.has(this.#current);
    const removedIndex = removedCurrent ? this.displayedIds().indexOf(this.#current as number) : -1;
    for (const [id, row] of this.#rows) {
      if (!keep.has(id)) {
        row.dispose();
        this.#rows.delete(id);
      }
    }
    for (const id of ids) {
      if (!this.#rows.has(id)) {
        this.#rows.set(id, new Message({
          view: this.#view,
          id,
          ...(this.#options.threadName !== undefined ? { threadName: this.#options.threadName } : {}),
          unknownAuthor: (partnerId) => this.#unknownAuthor(partnerId),
        }));
      }
    }
    this.#ids = ids;
    const display = this.displayedIds();
    const now = this.#view.services.now?.() ?? new Date();
    const nodes: Node[] = [];
    const pending = untrack(() => this.#syncPending());
    if (this.#order === "desc") nodes.push(...pending);
    let previousDay: string | null = null;
    const chronological = [...ids];
    const index = new Map(chronological.map((id, i) => [id, i] as const));
    for (const id of display) {
      const row = this.#rows.get(id) as Message;
      const data = row.data;
      let label: string | null = null;
      if (data !== undefined) {
        const key = dayKey(data.date, this.#view.services.timeZone);
        if (key !== previousDay) {
          previousDay = key;
          label = this.dayLabel(data.date, now);
        }
      }
      row.setDayLabel(label);
      const position = index.get(id) as number;
      const prior = position > 0 ? this.#rows.get(chronological[position - 1] as number)?.data : undefined;
      row.setSquashed(this.#squash && data !== undefined && this.isSquashed(data, prior));
      nodes.push(row.element);
    }
    if (this.#order === "asc") nodes.push(...pending);
    // Positions follow the order shown, unsent posts included (they are articles of the feed).
    nodes.forEach((node, at) => {
      (node as HTMLElement).setAttribute("aria-posinset", String(at + 1));
      (node as HTMLElement).setAttribute("aria-setsize", "-1");
    });
    const nothing = display.length === 0 && pending.length === 0;
    this.#empty.textContent = nothing ? this.emptyLabel() : "";
    this.#empty.hidden = !nothing;
    // An empty feed is hidden: a feed with no article would be an invalid structure.
    this.#feed.hidden = nodes.length === 0;
    reconcileChildren(this.#feed, nodes);
    this.#settleCurrent(display, removedCurrent, removedIndex, hadFocus);
    this.#restoreScroll(scroller, before, previousIds, ids);
  }

  // ---- internals

  #alive(): void {
    if (this.#disposed) throw new ThreadViewError("disposed", "this Thread was disposed");
  }

  #sync(): void {
    const view = untrack(() => this.#view);
    const windowView = view.store.windows.get(view.ref);
    const ids = windowView?.ids ?? [];
    const hasOlder = windowView?.hasOlder === true;
    pendingUids(view.store, view.ref);
    const failure = this.#failure();
    const loadingLatest = this.#loadingLatest();
    const loadingOlder = this.#loadingOlder();
    this.#renderStatus(failure, loadingLatest, windowView === undefined);
    this.#older.hidden = !hasOlder && !(failure?.what === "older");
    this.#older.setAttribute("aria-disabled", loadingOlder ? "true" : "false");
    this.#older.textContent = failure?.what === "older" ? t(view, "Retry loading older messages") : loadingOlder ? t(view, "Loading older messages") : t(view, "Load older messages");
    this.#feed.setAttribute("aria-busy", loadingLatest || loadingOlder ? "true" : "false");
    untrack(() => this.layout(ids));
  }

  #renderStatus(failure: { readonly what: "latest" | "older"; readonly text: string } | null, loading: boolean, noWindow: boolean): void {
    const view = this.#view;
    const doc = view.document;
    this.#status.replaceChildren();
    this.#status.removeAttribute("role");
    if (failure?.what === "latest") {
      this.#status.setAttribute("role", "alert");
      const retry = h(doc, "button", { type: "button", class: "erp-mail-retry" }, t(view, "Retry"));
      retry.addEventListener("click", () => void this.#loadLatest());
      this.#status.append(h(doc, "span", { class: "erp-mail-status-text" }, `${t(view, "Messages could not be loaded.")} ${failure.text}`), " ", retry);
    } else if (failure?.what === "older") {
      this.#status.setAttribute("role", "alert");
      this.#status.append(h(doc, "span", { class: "erp-mail-status-text" }, `${t(view, "Older messages could not be loaded.")} ${failure.text}`));
    } else if (loading && noWindow) {
      this.#status.setAttribute("role", "status");
      this.#status.append(h(doc, "span", { class: "erp-mail-status-text" }, t(view, "Loading messages")));
    }
  }

  async #loadLatest(): Promise<void> {
    if (this.#disposed || untrack(() => this.#loadingLatest())) return;
    this.#failure.set(null);
    this.#loadingLatest.set(true);
    try {
      await this.#view.pager.loadLatest(this.#view.ref);
    } catch (error) {
      this.#failure.set({ what: "latest", text: error instanceof Error ? error.message : String(error) });
      this.#view.announce(t(this.#view, "Messages could not be loaded."), "assertive");
      this.#view.reportError(error);
    } finally {
      this.#loadingLatest.set(false);
    }
  }

  async #loadOlder(): Promise<void> {
    if (this.#disposed || untrack(() => this.#loadingOlder())) return;
    const oldest = untrack(() => this.#view.store.windows.get(this.#view.ref)?.ids[0]);
    if (oldest === undefined) {
      this.#view.reportError(new ThreadViewError("bad_option", "Load older was activated for a thread with no loaded window"));
      return;
    }
    this.#failure.set(null);
    this.#loadingOlder.set(true);
    try {
      await this.#view.pager.loadOlder(this.#view.ref, oldest);
    } catch (error) {
      this.#failure.set({ what: "older", text: error instanceof Error ? error.message : String(error) });
      this.#view.announce(t(this.#view, "Older messages could not be loaded."), "assertive");
      this.#view.reportError(error);
    } finally {
      this.#loadingOlder.set(false);
    }
  }

  /** A missing author name is a diagnostic, never a blocking report: the row shows its placeholder. */
  #unknownAuthor(partnerId: number): void {
    if (this.#reportedAuthors.has(partnerId) || this.#reportedAuthors.size >= MAX_REPORTED_AUTHORS) return;
    this.#reportedAuthors.add(partnerId);
    console.warn(`res.partner#${partnerId} has no name in the store: its messages show "Partner #${partnerId}"`);
  }

  #syncPending(): HTMLElement[] {
    const view = this.#view;
    const uids = pendingUids(view.store, view.ref);
    const keep = new Set(uids);
    for (const [uid, row] of this.#pendingRows) {
      if (!keep.has(uid)) {
        row.dispose();
        this.#pendingRows.delete(uid);
      }
    }
    for (const uid of uids) {
      if (!this.#pendingRows.has(uid)) this.#pendingRows.set(uid, new PendingRow(view, uid));
    }
    const rows = uids.map((uid) => (this.#pendingRows.get(uid) as PendingRow).element);
    return this.#order === "desc" ? rows.reverse() : rows;
  }

  #atEnd(scroller: HTMLElement): boolean {
    return this.#order === "asc"
      ? scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < AT_END_PX
      : scroller.scrollTop < AT_END_PX;
  }

  #restoreScroll(scroller: HTMLElement, before: { height: number; top: number; atEnd: boolean }, previous: readonly number[], next: readonly number[]): void {
    const oldestBefore = previous[0];
    const oldestAfter = next[0];
    const newestBefore = previous[previous.length - 1];
    const newestAfter = next[next.length - 1];
    if (next.length === 0 || newestAfter === undefined) return;
    if (previous.length === 0) {
      // First load: anchor at the newest end.
      scroller.scrollTop = this.#order === "asc" ? scroller.scrollHeight : 0;
    } else if (oldestBefore !== undefined && oldestAfter !== undefined && oldestAfter < oldestBefore) {
      scroller.scrollTop = before.top + anchorDelta(this.#order, before.height, scroller.scrollHeight);
    } else if (newestBefore !== undefined && newestAfter > newestBefore && before.atEnd) {
      scroller.scrollTop = this.#order === "asc" ? scroller.scrollHeight : 0;
    }
  }

  #setCurrent(id: number | null): void {
    this.#current = id;
    for (const [rowId, row] of this.#rows) {
      row.element.tabIndex = rowId === id ? 0 : -1;
      row.setActive(rowId === id);
    }
  }

  #settleCurrent(display: readonly number[], removedCurrent: boolean, removedIndex: number, hadFocus: boolean): void {
    if (display.length === 0) {
      this.#current = null;
      if (hadFocus) {
        this.element.tabIndex = -1;
        this.element.focus();
      }
      return;
    }
    if (this.#current === null || !this.#rows.has(this.#current)) {
      const fallback = display[Math.min(Math.max(removedIndex, 0), display.length - 1)] as number;
      this.#setCurrent(removedCurrent ? fallback : display[0] as number);
      if (removedCurrent && hadFocus) this.#rows.get(fallback)?.element.focus();
    } else {
      this.#setCurrent(this.#current);
    }
  }

  #onFocusIn(event: FocusEvent): void {
    const article = (event.target as HTMLElement).closest<HTMLElement>("article[data-message-id]");
    if (article === null) return;
    const id = Number(article.dataset["messageId"]);
    if (id !== this.#current) this.#setCurrent(id);
  }

  #onKeydown(event: KeyboardEvent): void {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const target = event.target as HTMLElement;
    const article = target.closest<HTMLElement>("article[data-message-id]");
    if (article === null) return;
    if (target !== article) {
      if (event.key === "Escape") {
        event.preventDefault();
        article.focus();
      }
      return;
    }
    const display = this.displayedIds();
    const index = display.indexOf(Number(article.dataset["messageId"]));
    let next = index;
    if (event.key === "ArrowDown" || event.key === "PageDown") next = Math.min(display.length - 1, index + 1);
    else if (event.key === "ArrowUp" || event.key === "PageUp") next = Math.max(0, index - 1);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = display.length - 1;
    else return;
    event.preventDefault();
    const id = display[next];
    if (id !== undefined) this.focusMessage(id);
  }
}

/** The row of one unsent post: its text, its state, and Retry and Discard while it failed. */
class PendingRow {
  readonly element: HTMLElement;
  readonly #stop: () => void;

  constructor(view: ThreadViewContext, uid: string) {
    const doc = view.document;
    // An article, so that the feed holds nothing else.
    this.element = h(doc, "article", { class: "erp-mail-pending", "data-uid": uid, "aria-label": t(view, "Unsent message") });
    const models = uiModels(view.store);
    this.#stop = effect(() => {
      if (!models.pending.has(uid)) return;
      const state = models.pending.get(uid, "state");
      const error = models.pending.get(uid, "error");
      const text = String(models.pending.get(uid, "text"));
      const parts: Node[] = [h(doc, "p", { class: "erp-mail-pending-text" }, text)];
      if (state === "failed") {
        const retry = h(doc, "button", { type: "button", class: "erp-mail-retry" }, t(view, "Retry"));
        const discard = h(doc, "button", { type: "button", class: "erp-mail-discard" }, t(view, "Discard"));
        retry.addEventListener("click", () => void sendPending(view, uid));
        discard.addEventListener("click", () => discardPending(view, uid));
        parts.push(h(doc, "p", { class: "erp-mail-pending-error", role: "alert" }, `${t(view, "The message was not sent.")} ${typeof error === "string" ? error : ""}`.trim()), h(doc, "div", { class: "erp-mail-pending-actions" }, retry, " ", discard));
        this.element.classList.add("is-failed");
      } else {
        parts.push(h(doc, "p", { class: "erp-mail-pending-state" }, t(view, "Sending")));
        this.element.classList.remove("is-failed");
      }
      this.element.replaceChildren(...parts);
    });
  }

  dispose(): void {
    this.#stop();
    this.element.remove();
  }
}

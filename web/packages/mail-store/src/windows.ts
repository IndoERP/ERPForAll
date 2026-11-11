/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { KeyedRecordStore } from "@erpforall/elang";
import { MessagingStoreError } from "./errors";

/** A thread: the record (model, id) whose messages a window lists (2). */
export interface ThreadRef {
  readonly model: string;
  readonly id: number;
}

/**
 * One thread's loaded messages: a single contiguous run of message ids in ascending order. `hasOlder` and `hasNewer`
 * say that messages exist beyond the run on that side (the run was capped or the server has more).
 */
export interface WindowView {
  readonly ids: readonly number[];
  readonly hasOlder: boolean;
  readonly hasNewer: boolean;
}

/**
 * What {@link ThreadWindows.appendLive} did with a message that arrived on the bus:
 * `appended` (new newest), `inserted` (inside the run), `prepended` (below the run of a thread whose older side is
 * fully loaded), `duplicate` (already in the run), `gap` (the run does not reach the newest message, so the message
 * arrives with the next fetch), `older` (below the run while older messages exist: the older pages bring it),
 * `no_window` (the thread has no window: nothing is held for it).
 */
export type LiveOutcome = "appended" | "inserted" | "prepended" | "duplicate" | "gap" | "older" | "no_window";

const WINDOW_TABLE = "$messaging.window";

function key(ref: ThreadRef): string {
  return `${ref.model}\0${ref.id}`;
}

function label(ref: ThreadRef): string {
  return `${ref.model}#${ref.id}`;
}

/**
 * The message windows of a store (4). Each window is one contiguous run capped at
 * `pageSize * maxWindowPages` messages: a page that would overflow the cap trims the far side and sets its flag, so
 * a window is never two runs with a hole between them.
 *
 * The window state lives in a reserved table of the store's keyed tables, so a thread view that reads
 * {@link ThreadWindows.get} inside a reactive scope updates when the run changes.
 */
export class ThreadWindows {
  readonly #keyed: KeyedRecordStore;
  readonly #max: number;
  readonly #refs = new Map<string, ThreadRef>();
  readonly #touched = new Map<string, number>();
  #clock = 0;

  /** @internal Created by the store. */
  constructor(keyed: KeyedRecordStore, maxMessages: number) {
    this.#keyed = keyed;
    this.#max = maxMessages;
  }

  /** The most messages one window holds. */
  get maxMessages(): number {
    return this.#max;
  }

  /**
   * The thread's window, tracked inside a reactive scope; `undefined` when the thread has none (never loaded, or
   * evicted: fetch the latest page and call {@link ThreadWindows.replace}).
   *
   * @param ref - the thread
   */
  get(ref: ThreadRef): WindowView | undefined {
    return this.#view(ref);
  }

  /**
   * Install the latest page as the whole window, discarding any earlier run.
   *
   * @param ref - the thread
   * @param ids - the page's message ids, strictly ascending (may be empty for a thread with no messages)
   * @param hasOlder - whether older messages exist on the server
   * @throws {@link MessagingStoreError} `window` for ids that are not strictly ascending positive integers
   */
  replace(ref: ThreadRef, ids: readonly number[], hasOlder: boolean): void {
    this.#checkIds(ref, "replace", ids);
    let run = [...ids];
    let older = hasOlder;
    if (run.length > this.#max) {
      run = run.slice(run.length - this.#max);
      older = true;
    }
    this.#write(ref, run, older, false);
  }

  /**
   * Add an older page below the run. The page must lie strictly below the run's oldest id, and the run must have
   * older messages.
   *
   * @param ref - the thread
   * @param ids - the page, strictly ascending, non-empty
   * @param hasOlder - whether still older messages exist
   * @throws {@link MessagingStoreError} `window` when there is no window, no older side, or the page overlaps the run
   */
  prependOlder(ref: ThreadRef, ids: readonly number[], hasOlder: boolean): void {
    const held = this.#require(ref, "prependOlder");
    this.#checkIds(ref, "prependOlder", ids);
    if (!held.hasOlder) {
      throw this.#error(ref, "prependOlder", "the window has no older messages to load");
    }
    const oldest = held.ids[0];
    const newest = ids[ids.length - 1];
    if (ids.length === 0 || (oldest !== undefined && newest !== undefined && newest >= oldest)) {
      throw this.#error(ref, "prependOlder", `the page must be non-empty and end below the window's oldest id ${oldest ?? "none"}`);
    }
    let run = [...ids, ...held.ids];
    let newer = held.hasNewer;
    if (run.length > this.#max) {
      run = run.slice(0, this.#max);
      newer = true;
    }
    this.#write(ref, run, hasOlder, newer);
  }

  /**
   * Add a newer page above the run (after the run was trimmed on its newer side). The page must lie strictly above
   * the run's newest id, and the run must have newer messages.
   *
   * @param ref - the thread
   * @param ids - the page, strictly ascending, non-empty
   * @param hasNewer - whether still newer messages exist
   * @throws {@link MessagingStoreError} `window` when there is no window, no newer side, or the page overlaps the run
   */
  appendNewer(ref: ThreadRef, ids: readonly number[], hasNewer: boolean): void {
    const held = this.#require(ref, "appendNewer");
    this.#checkIds(ref, "appendNewer", ids);
    if (!held.hasNewer) {
      throw this.#error(ref, "appendNewer", "the window has no newer messages to load");
    }
    const newest = held.ids[held.ids.length - 1];
    const first = ids[0];
    if (ids.length === 0 || (newest !== undefined && first !== undefined && first <= newest)) {
      throw this.#error(ref, "appendNewer", `the page must be non-empty and start above the window's newest id ${newest ?? "none"}`);
    }
    let run = [...held.ids, ...ids];
    let older = held.hasOlder;
    if (run.length > this.#max) {
      run = run.slice(run.length - this.#max);
      older = true;
    }
    this.#write(ref, run, older, hasNewer);
  }

  /**
   * A message arrived on the bus. It joins the run only where that keeps the run contiguous (see
   * {@link LiveOutcome}); nothing is created for a thread without a window.
   *
   * @param ref - the thread
   * @param id - the message id
   */
  appendLive(ref: ThreadRef, id: number): LiveOutcome {
    if (!Number.isSafeInteger(id) || id < 1) {
      throw this.#error(ref, "appendLive", `message id must be a positive safe integer, got ${String(id)}`);
    }
    const held = this.#view(ref);
    if (held === undefined) return "no_window";
    const first = held.ids[0];
    const last = held.ids[held.ids.length - 1];
    if (held.ids.includes(id)) {
      this.#touch(ref);
      return "duplicate";
    }
    if (held.hasNewer && (last === undefined || id > last)) return "gap";
    if (last === undefined || first === undefined || id > last) {
      return this.#extend(ref, held, [...held.ids, id], "appended");
    }
    if (id > first) {
      const run = [...held.ids, id].sort((a, b) => a - b);
      return this.#extend(ref, held, run, "inserted");
    }
    if (held.hasOlder) return "older";
    return this.#extend(ref, held, [id, ...held.ids], "prepended");
  }

  /**
   * Remove a deleted message from every window that lists it.
   *
   * @param id - the message id
   * @returns the number of windows changed
   */
  remove(id: number): number {
    let changed = 0;
    for (const ref of this.#refs.values()) {
      const held = this.#view(ref);
      if (held === undefined || !held.ids.includes(id)) continue;
      this.#write(ref, held.ids.filter((m) => m !== id), held.hasOlder, held.hasNewer, false);
      changed++;
    }
    return changed;
  }

  /** The threads that have a window. */
  threads(): ThreadRef[] {
    return [...this.#refs.values()];
  }

  /** Logical time of the last use of a window (larger is more recent); `collect()` orders by it. */
  lastUsed(ref: ThreadRef): number {
    return this.#touched.get(key(ref)) ?? 0;
  }

  /**
   * Use of a thread by a surface counts as recent use, for the least-recently-used rule.
   *
   * @param ref - the thread
   */
  touch(ref: ThreadRef): void {
    if (this.#refs.has(key(ref))) this.#touch(ref);
  }

  /** Drops a window (eviction, used by `collect()`). */
  drop(ref: ThreadRef): void {
    const k = key(ref);
    if (!this.#refs.delete(k)) return;
    this.#touched.delete(k);
    this.#keyed.table(WINDOW_TABLE).deleteRecord(k);
  }

  /** Every message id any window lists (used by `collect()`). */
  messageIds(): Set<number> {
    const out = new Set<number>();
    for (const ref of this.#refs.values()) {
      for (const id of this.#view(ref)?.ids ?? []) out.add(id);
    }
    return out;
  }

  /** Drops every window (a new schema generation). */
  clear(): void {
    for (const ref of [...this.#refs.values()]) this.drop(ref);
  }

  #extend(ref: ThreadRef, held: WindowView, run: number[], outcome: LiveOutcome): LiveOutcome {
    let older = held.hasOlder;
    if (run.length > this.#max) {
      run = run.slice(run.length - this.#max);
      older = true;
    }
    this.#write(ref, run, older, held.hasNewer);
    return outcome;
  }

  #view(ref: ThreadRef): WindowView | undefined {
    const k = key(ref);
    const table = this.#keyed.table(WINDOW_TABLE);
    const ids = table.get(k, "ids");
    if (ids === undefined) return undefined;
    return { ids: ids as readonly number[], hasOlder: table.get(k, "hasOlder") === true, hasNewer: table.get(k, "hasNewer") === true };
  }

  #require(ref: ThreadRef, method: string): WindowView {
    const held = this.#view(ref);
    if (held === undefined) throw this.#error(ref, method, "the thread has no window (never loaded or evicted): load the latest page first");
    return held;
  }

  #write(ref: ThreadRef, ids: readonly number[], hasOlder: boolean, hasNewer: boolean, touch = true): void {
    const k = key(ref);
    const table = this.#keyed.table(WINDOW_TABLE);
    this.#refs.set(k, ref);
    table.set(k, "ids", Object.freeze([...ids]));
    table.set(k, "hasOlder", hasOlder);
    table.set(k, "hasNewer", hasNewer);
    if (touch) this.#touch(ref);
  }

  #touch(ref: ThreadRef): void {
    this.#touched.set(key(ref), ++this.#clock);
  }

  #checkIds(ref: ThreadRef, method: string, ids: readonly number[]): void {
    let previous = 0;
    for (const [index, id] of ids.entries()) {
      if (!Number.isSafeInteger(id) || id < 1) {
        throw this.#error(ref, method, `ids[${index}] must be a positive safe integer, got ${String(id)}`);
      }
      if (id <= previous) throw this.#error(ref, method, `ids must be strictly ascending, ids[${index}] = ${id} follows ${previous}`);
      previous = id;
    }
  }

  #error(ref: ThreadRef, method: string, message: string): MessagingStoreError {
    return new MessagingStoreError("window", `ThreadWindows(${label(ref)}).${method}: ${message}`);
  }
}

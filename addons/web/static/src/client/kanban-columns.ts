/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Grouped board and paging adapted from ERPForAll framework commit b8820ae6f5.
import { el } from "./dom.js";
import { icon } from "./icons.js";
import { decodeGroups } from "./list-groups.js";
import { kanbanCard, type KanbanCardsOptions, type KanbanRendered } from "./kanban.js";
import type { KanbanColumnKey, KanbanFoldState } from "./kanban-fold.js";
import type { GroupableField, ListAggregate } from "./search/types.js";
import { ClientError } from "./transport.js";

/** One column as the grouped read answers it. `records` is present when the server opened the column. */
export interface KanbanColumnData { key: string; label: string; count: number; domain: unknown[]; value: KanbanColumnKey; sum?: number; records?: Record<string, unknown>[] }

const badRecords = (): never => { throw new ClientError("server", "Invalid kanban column records."); };

/** Validates a `web_read_group` answer asked with `auto_unfold` and a card specification, grouped by `spec` of `field`:
 *  the list's closed group shape (at most `limit` columns), plus the first cards of each column the server opened
 *  (`__records`): records with distinct ids, never more than the column counts nor than `maxCards`, the most any column
 *  was asked for. The column of the unset value comes first, as the reference draws it. */
export function decodeKanbanGroups(raw: unknown, field: GroupableField, spec: string, limit: number, maxCards: number, sum?: ListAggregate): { columns: KanbanColumnData[]; length: number } {
  const answer = raw && typeof raw === "object" && !Array.isArray(raw) ? raw as Record<string, unknown> : undefined;
  const rows = Array.isArray(answer?.["groups"]) ? answer["groups"] as unknown[] : undefined;
  const cards: (Record<string, unknown>[] | undefined)[] = [];
  // Without its cards a row is the list's group; the list's admission refuses everything else (unknown keys included).
  const plain = rows?.map(item => {
    if (!item || typeof item !== "object" || Array.isArray(item)) { cards.push(undefined); return item; }
    const { __records: records, ...rest } = item as Record<string, unknown>;
    if (records === undefined) { cards.push(undefined); return rest; }
    const count = rest["__count"];
    if (!Array.isArray(records) || typeof count !== "number" || records.length > count || records.length > maxCards) return badRecords();
    const ids = new Set<unknown>();
    for (const record of records) {
      const id = record && typeof record === "object" && !Array.isArray(record) ? (record as Record<string, unknown>)["id"] : undefined;
      if (typeof id !== "number" || !Number.isSafeInteger(id) || id <= 0 || ids.has(id)) return badRecords();
      ids.add(id);
    }
    cards.push(records as Record<string, unknown>[]);
    return rest;
  });
  const decoded = decodeGroups(plain === undefined ? raw : { ...answer, groups: plain }, field, limit, { spec, aggregates: sum ? [sum] : [] });
  const columns = decoded.groups.map((group, index): KanbanColumnData => {
    const records = cards[index];
    const total = sum ? group.aggregates?.[`${sum.field}:sum`] : null;
    return { ...(sum ? {sum: typeof total === "number" ? total : 0} : {}), key: group.key, label: group.label, count: group.count, domain: group.domain, value: group.value as KanbanColumnKey, ...(records ? { records } : {}) };
  });
  const unset = columns.findIndex(item => item.value === false);
  if (unset > 0) columns.unshift(...columns.splice(unset, 1));
  return { columns, length: decoded.length };
}

/** What the grouped read is told about the user's columns (`opening_info`): the ones the user folded stay folded, the ones
 *  the user opened are opened with `pageSize` cards, and a column that showed more cards than a page (`loaded`, by its
 *  value) is read with that many. Every other column follows the server's own rule. Nothing to tell: no argument. */
export function kanbanOpeningInfo(state: KanbanFoldState | undefined, pageSize: number, loaded?: ReadonlyMap<KanbanColumnKey, number>): Record<string, unknown>[] | undefined {
  const same = (value: KanbanColumnKey): string => JSON.stringify(value);
  const folded = new Set((state?.folded ?? []).map(same)), named = new Set<string>();
  const info: Record<string, unknown>[] = (state?.folded ?? []).map(value => ({ value, folded: true }));
  // The server requires `limit` and `offset` on an opened entry (crates/erp-orm/src/web/groups.rs `open_groups`).
  const opened = (value: KanbanColumnKey): void => { named.add(same(value)); info.push({ value, folded: false, limit: Math.max(pageSize, loaded?.get(value) ?? 0), offset: 0 }); };
  for (const value of state?.opened ?? []) opened(value);
  for (const [value, count] of loaded ?? []) if (count > pageSize && !folded.has(same(value)) && !named.has(same(value))) opened(value);
  return info.length ? info : undefined;
}

/** One column as the board draws it; the application changes it and calls `refresh(key)`. */
export interface KanbanColumnState {
  key: string; label: string; count: number; folded: boolean;
  /** The cards loaded so far, in order. */
  records: Record<string, unknown>[];
  /** A read of this column is under way. */
  busy: boolean;
  /** The sentence shown in the column when its last read failed. */
  error?: string;
}
export interface KanbanBoardOptions extends Pick<KanbanCardsOptions, "layout" | "renderField" | "open"> {
  columns: readonly KanbanColumnState[];
  /** The most cards one column holds (setting `web.kanban_max_column_cards`); past it the column says so instead of loading more. */
  maxCards: number;
  /** Optional controls receive the same column state as paging and folding. */
  headerControls?: (column: KanbanColumnState) => HTMLElement;
  move?: (id: number, target: KanbanColumnState, card: HTMLElement) => void;
  onToggle: (column: KanbanColumnState) => void;
  onLoadMore: (column: KanbanColumnState) => void;
  onRetry: (column: KanbanColumnState) => void;
}
export interface KanbanBoard extends KanbanRendered {
  /** Redraws the column `key` from its state; focus stays in the column (on the same card, or on the first card that arrived). */
  refresh: (key: string) => void;
}

const records = (count: number): string => count === 1 ? "1 record" : `${count} records`;
let boards = 0;

/** The board: one column per group, side by side. An open column has a header (title, count, fold), its cards and what
 *  follows them (load more, the bound, an empty sentence or its own failure); a folded column is one control. */
export function renderKanbanBoard(options: KanbanBoardOptions): KanbanBoard {
  const { columns, maxCards, open, onToggle, onLoadMore, onRetry } = options;
  const board = el("div", undefined, "kanban-board");
  board.setAttribute("role", "group");
  board.setAttribute("aria-label", "Columns");
  const prefix = `kanban-board-${++boards}`;
  const stops = new Map<string, (() => void)[]>();
  const nodes = new Map<string, HTMLElement>();
  const allCards = (): HTMLElement[] => [...board.querySelectorAll<HTMLElement>(".kanban-card")];
  const focusCard = (card: HTMLElement): void => {
    for (const other of allCards()) other.tabIndex = other === card ? 0 : -1;
    card.focus();
  };
  // One card of the board is in the Tab order (the first, until the user moves).
  const keepTabStop = (): void => {
    const cards = allCards();
    if (cards.length && !cards.some(card => card.tabIndex === 0)) cards[0]!.tabIndex = 0;
  };
  const action = (label: string, className: string, run: () => void): HTMLButtonElement => {
    const node = el("button", label, className);
    node.type = "button";
    node.addEventListener("click", run);
    return node;
  };
  let dragging: HTMLElement | undefined;
  const moveCard = (card: HTMLElement, target: KanbanColumnState): void => {
    if (card.closest<HTMLElement>(".kanban-column")?.dataset["key"] !== target.key) {
      options.move?.(Number(card.dataset["id"]), target, card);
    }
  };
  const draw = (column: KanbanColumnState, index: number): HTMLElement => {
    for (const stop of stops.get(column.key) ?? []) stop();
    const held: (() => void)[] = [];
    stops.set(column.key, held);
    const node = el("div", undefined, `kanban-column${column.folded ? " is-folded" : ""}`);
    node.setAttribute("role", "group");
    node.setAttribute("aria-label", `${column.label}, ${records(column.count)}`);
    node.dataset["key"] = column.key;
    if (options.move) {
      node.addEventListener("dragover", event => { if (dragging) event.preventDefault(); });
      node.addEventListener("drop", event => {
        if (!dragging) return;
        event.preventDefault(); moveCard(dragging, column); dragging = undefined;
      });
    }
    if (column.folded) {
      const unfold = action("", "kanban-column-unfold", () => onToggle(column));
      unfold.setAttribute("aria-expanded", "false");
      unfold.setAttribute("aria-label", `Unfold column ${column.label}, ${records(column.count)}`);
      unfold.title = `Unfold ${column.label}`;
      unfold.append(el("span", column.label, "kanban-column-name"), el("span", String(column.count), "kanban-column-count"));
      node.append(unfold);
      return node;
    }
    const bodyId = `${prefix}-column-${index}`;
    const head = el("div", undefined, "kanban-column-head");
    const title = el("h2", column.label, "kanban-column-title");
    title.title = column.label;
    const fold = action("", "kanban-column-fold", () => onToggle(column));
    fold.append(icon("chevron-left"));
    fold.setAttribute("aria-expanded", "true");
    fold.setAttribute("aria-controls", bodyId);
    fold.setAttribute("aria-label", `Fold column ${column.label}`);
    fold.title = "Fold";
    head.append(title, el("span", String(column.count), "kanban-column-count"), fold);
    if (options.headerControls) head.append(options.headerControls(column));
    const body = el("div", undefined, "kanban-column-body");
    body.id = bodyId;
    if (column.busy) body.setAttribute("aria-busy", "true");
    if (column.records.length) {
      const list = el("div", undefined, "kanban-column-cards");
      list.setAttribute("role", "list");
      list.setAttribute("aria-label", `${column.label} records`);
      for (const record of column.records) {
        const card = kanbanCard(options, record, held);
        if (options.move && columns.length > 1) {
          card.draggable = true;
          card.addEventListener("dragstart", event => {
            dragging = card;
            event.dataTransfer?.setData("text/plain", String(record["id"]));
          });
          card.addEventListener("dragend", () => { dragging = undefined; });
          const select = el("select", undefined, "kanban-move");
          select.setAttribute("aria-label", `Move ${card.getAttribute("aria-label")} to column`);
          select.append(new Option("Move to…", ""));
          for (const target of columns) if (target !== column) select.append(new Option(target.label, target.key));
          select.addEventListener("change", () => {
            const target = columns.find(item => item.key === select.value);
            select.selectedIndex = 0;
            if (target) moveCard(card, target);
          });
          card.append(select);
        }
        list.append(card);
      }
      body.append(list);
    }
    const left = column.count - column.records.length;
    if (column.error !== undefined) {
      const alert = el("div", undefined, "kanban-column-error");
      alert.setAttribute("role", "alert");
      alert.append(el("p", column.error), action("Try again", "kanban-column-retry", () => onRetry(column)));
      body.append(alert);
    } else if (left > 0 && column.records.length >= maxCards) {
      body.append(el("p", `Showing the first ${column.records.length} of ${records(column.count)}. Narrow the search to see the others.`, "kanban-column-bound muted"));
    } else if (column.busy && !column.records.length) {
      body.append(el("p", "Loading records", "kanban-column-loading muted"));
    } else if (left > 0) {
      const more = action(`Load more (${left} left)`, "kanban-load-more", () => onLoadMore(column));
      more.disabled = column.busy;
      body.append(more);
    } else if (!column.records.length) {
      body.append(el("p", "No records", "kanban-column-empty muted"));
    }
    node.append(head, body);
    return node;
  };
  columns.forEach((column, index) => { const node = draw(column, index); nodes.set(column.key, node); board.append(node); });
  keepTabStop();

  const refresh = (key: string): void => {
    const index = columns.findIndex(column => column.key === key), previous = nodes.get(key);
    if (index < 0 || !previous) throw new ClientError("validation", "This kanban column is no longer on the board.");
    const active = document.activeElement instanceof HTMLElement && previous.contains(document.activeElement) ? document.activeElement : undefined;
    const before = previous.querySelectorAll(".kanban-card").length, hadStop = previous.querySelector('.kanban-card[tabindex="0"]')?.getAttribute("data-id");
    const node = draw(columns[index]!, index);
    nodes.set(key, node);
    previous.replaceWith(node);
    const cards = [...node.querySelectorAll<HTMLElement>(".kanban-card")];
    const kept = hadStop ? cards.find(card => card.dataset["id"] === hadStop) : undefined;
    if (kept) kept.tabIndex = 0;
    keepTabStop();
    if (!active) return;
    // Focus stays in the column: the same card, the first card a load brought, or the column's own control.
    const control = (): HTMLElement | null => node.querySelector<HTMLElement>(".kanban-load-more, .kanban-column-retry, .kanban-column-fold, .kanban-column-unfold");
    const toggle = node.querySelector<HTMLElement>(".kanban-column-fold, .kanban-column-unfold");
    if (active.classList.contains("kanban-card")) {
      const same = cards.find(card => card.dataset["id"] === active.dataset["id"]) ?? cards[0];
      if (same) focusCard(same); else control()?.focus();
    } else if (active.classList.contains("kanban-column-fold") || active.classList.contains("kanban-column-unfold")) toggle?.focus();
    else if (cards.length > before) focusCard(cards[before]!);
    else control()?.focus();
  };

  const onKey = (event: KeyboardEvent): void => {
    const card = (event.target as HTMLElement).closest<HTMLElement>(".kanban-card");
    if (!card || event.target !== card || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key === "Enter" && open) { event.preventDefault(); open(Number(card.dataset["id"])); return; }
    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    // Columns that show cards, in board order; folded and empty ones are passed over (the reference does the same).
    const lists = [...board.querySelectorAll<HTMLElement>(".kanban-column")].map(column => [...column.querySelectorAll<HTMLElement>(".kanban-card")]).filter(list => list.length);
    const at = lists.findIndex(list => list.includes(card)), own = lists[at]!, row = own.indexOf(card);
    const target = event.key === "ArrowUp" ? own[row - 1] : event.key === "ArrowDown" ? own[row + 1] : event.key === "Home" ? own[0] : event.key === "End" ? own[own.length - 1]
      : lists[at + (event.key === "ArrowLeft" ? -1 : 1)]?.[0];
    if (target) focusCard(target);
  };
  board.addEventListener("keydown", onKey);
  return {
    node: board, refresh,
    dispose: () => { board.removeEventListener("keydown", onKey); for (const held of stops.values()) for (const stop of held.splice(0)) stop(); stops.clear(); }
  };
}

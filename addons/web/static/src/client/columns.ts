/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";

/** Widest content-sized flexible column on first draw, before the columns are fitted to the list. */
const AUTO_COLUMN_MAX = 480;
/** Narrowest flexible column, and the narrowest the record's name column (the first text column) is drawn: a crowded
 *  list scrolls sideways before the name becomes unreadable. */
const FLEX_MIN = 80, NAME_MIN = 160;
/** Left and right padding of a list cell (client.css `--list-cell-x`): an arch `width` names the content width, as in
 *  the reference, and the column is that plus its padding. */
const CELL_PADDING = 24;
// The last sortable header shares a cell with the chooser. In the touch layout both controls
// need 44px, with 6px between them, 16px for the resize edge and the normal leading padding.
const CHOOSER_HEADER_MIN = CELL_PADDING / 2 + 44 + 6 + 44 + 16;
/**
  Width rules by field kind or widget, border box in px (cell padding included). A `fixed` column takes the width of
 *  its widest value and header, kept between the bounds, and never grows with the list: a number, a date or a check box
 *  needs no more room on a wide screen. Other rules flex within their bounds (`hi` absent: no upper bound).
 */
type WidthRule = { fixed: boolean; lo: number; hi?: number };
const WIDTH_RULES: Readonly<Record<string, WidthRule>> = {
  integer: { fixed: true, lo: 64, hi: 140 },
  float: { fixed: true, lo: 80, hi: 180 },
  monetary: { fixed: true, lo: 80, hi: 180 },
  date: { fixed: true, lo: 96, hi: 200 },
  datetime: { fixed: true, lo: 140, hi: 240 },
  priority: { fixed: true, lo: 64, hi: 140 },
  color: { fixed: true, lo: 48, hi: 96 },
  handle: { fixed: true, lo: 40, hi: 48 },
  boolean: { fixed: false, lo: 56, hi: 100 },
  boolean_favorite: { fixed: false, lo: 56, hi: 100 },
  boolean_icon: { fixed: false, lo: 56, hi: 100 },
  boolean_toggle: { fixed: false, lo: 56, hi: 100 },
  text: { fixed: false, lo: FLEX_MIN, hi: 600 },
  html: { fixed: false, lo: FLEX_MIN, hi: 600 },
};
/** A list column as the list view describes it. `width` comes from the arch (`width="100px"` fixed, `"[100px]"` a
 *  minimum, `"[100px,200px]"` both, content width without padding) and wins over `widget` and `kind`, which choose
 *  the built-in rule (absent: flexible). */
export type ListColumn = { name: string; label: string; kind?: string; widget?: string; width?: { min: number; max?: number } };
const NAME_KINDS = new Set(["char", "many2one"]);
function ruleOf(column: ListColumn): WidthRule | undefined {
  if (column.width) {
    const lo = column.width.min + CELL_PADDING, hi = column.width.max === undefined ? undefined : column.width.max + CELL_PADDING;
    return hi === lo ? { fixed: true, lo, hi } : hi === undefined ? { fixed: false, lo } : { fixed: false, lo, hi };
  }
  return (column.widget ? WIDTH_RULES[column.widget] : undefined) ?? (column.kind ? WIDTH_RULES[column.kind] : undefined);
}
/** Fits measured widths to the room: each column first kept within its own bounds, then flexible columns (no upper
 *  bound, or below it) share any spare room evenly, or columns above their minimum give up the excess evenly. When the
 *  minimums alone are wider than the room the columns stay at their minimum and the list scrolls sideways. */
export function fitColumns(columns: readonly { width: number; min: number; max?: number }[], room: number): number[] {
  const widths = columns.map(({width, min, max}) => Math.max(min, max === undefined ? width : Math.min(width, max)));
  if (!(room > 0)) return widths;
  let diff = widths.reduce((sum, width) => sum + width, 0) - room;
  if (diff >= 1) {
    let open = columns.map((_, i) => i).filter(i => widths[i]! > columns[i]!.min);
    const spare = open.reduce((sum, i) => sum + widths[i]! - columns[i]!.min, 0);
    if (diff >= spare) for (const i of open) widths[i] = columns[i]!.min;
    else while (diff >= 0.5 && open.length) {
      const step = diff / open.length;
      for (const i of open) { const next = Math.max(columns[i]!.min, widths[i]! - step); diff -= widths[i]! - next; widths[i] = next; }
      open = open.filter(i => widths[i]! > columns[i]!.min);
    }
  } else if (diff <= -1) {
    let room2 = -diff;
    let open = columns.map((_, i) => i).filter(i => columns[i]!.max === undefined || widths[i]! < columns[i]!.max!);
    while (room2 >= 0.5 && open.length) {
      const step = room2 / open.length;
      for (const i of open) { const cap = columns[i]!.max ?? Infinity, next = Math.min(cap, widths[i]! + step); room2 -= next - widths[i]!; widths[i] = next; }
      open = open.filter(i => columns[i]!.max === undefined || widths[i]! < columns[i]!.max!);
    }
  }
  return widths.map(width => Math.round(width));
}
/** Frames to wait for an off-screen list to be mounted before giving up on content sizing (about 2 s at 60 Hz). */
const AUTO_LAYOUT_FRAMES = 120;
/**
  How long after its first measure a list measures again when the content of its cells changes.
 *  After this time the layout stays as it is, so an
 *  edit or a new page never moves the columns. 
 */
const LATE_CONTENT_MS = 8000;

/** Columns the user resized by hand, per widths map (so they survive a redraw): they keep a fixed width, the spare room
 *  goes to the last flexible column that was not. When every column is pinned the table is as wide as its columns. */
const pinned = new WeakMap<Map<string, number>, Set<string>>();
type Spec = { width: number; min: number; max?: number };
/** What the last measure found, per widths map: each automatic column's rule-bound spec (so a resized window refits
 *  without measuring again), whether the list had no record then, and the room the columns were fitted to. */
const measures = new WeakMap<Map<string, number>, { specs: Map<string, Spec>; empty: boolean; room: number; at?: number }>();
const wideWatchers = new WeakMap<Element, ResizeObserver>();
/** A table wider than its list (columns widened by hand) scrolls sideways; otherwise the list does not scroll, so the
 *  header can stick to the page (client.css). The slack covers the rounding of the fitted widths, one pixel per column. */
function flagWide(table: HTMLTableElement): void {
  const box = table.parentElement;
  box?.toggleAttribute("data-wide", table.offsetWidth > (box?.clientWidth ?? 0) + table.querySelectorAll("col").length);
}

/** Pointer capture keeps resize listeners inside the table's own lifecycle. Widths live only in this list view. */
/** Returns `settle`: the owner calls it right after attaching the table, so the first-draw content sizing runs in the
 *  same task and the content layout is never painted. A table attached later settles on an animation frame. */
/** `selector`: the list starts with the 40 px row-selection column (the main list); `actions`: it ends with a 40 px
 *  actions column (a sub-list's remove buttons). */
export type ColumnFrame = { selector: boolean; actions: boolean };
export function resizeColumns(table: HTMLTableElement, columns: readonly ListColumn[], widths: Map<string, number>, frame: ColumnFrame = { selector: true, actions: false }): () => void {
  const row = table.tHead?.rows[0];
  if (!row) throw new Error("Resizable columns require a table header.");
  const group = el("colgroup");
  if (frame.selector) { const selector = el("col"); selector.style.width = "40px"; group.append(selector); }
  // Room the data columns cannot use: the selector and actions columns.
  const side = (frame.selector ? 40 : 0) + (frame.actions ? 40 : 0), offset = frame.selector ? 1 : 0;
  const nameIndex = columns.findIndex(column => column.kind !== undefined && NAME_KINDS.has(column.kind) && !ruleOf(column));
  const entries = columns.map((field, index) => {
    const head = row.cells[index + offset];
    if (!head) throw new Error(`Missing header for ${field.name}.`);
    const col = el("col"); group.append(col);
    const rule = ruleOf(field);
    // The narrowest this column is drawn when the list is fitted; a manual resize may go down to 80 px or this floor.
    const controlsMin = head.matches(".sortable-column.column-choices-anchor") ? CHOOSER_HEADER_MIN : 0;
    const floor = Math.max(controlsMin, rule?.lo ?? (index === nameIndex ? NAME_MIN : FLEX_MIN));
    const flexible = !rule || !rule.fixed && rule.hi === undefined;
    // One-line content width bound by the rule: a fixed column keeps it, a flexible one starts from it (capped).
    const spec = (measured: number): Spec => {
      if (rule?.fixed) { const width = Math.max(floor, Math.min(rule.hi ?? floor, measured)); return { width, min: width, max: width }; }
      if (rule?.hi !== undefined) return { width: measured, min: floor, max: Math.max(floor, rule.hi) };
      return { width: Math.min(AUTO_COLUMN_MAX, measured), min: floor };
    };
    const manualMin = Math.max(controlsMin, Math.min(floor, FLEX_MIN));
    const clamp = (value: number): number => Math.max(manualMin, Math.min(4096, Math.round(value)));
    return { field, head, col, floor, controlsMin, flexible, spec, manualMin, clamp };
  });
  if (frame.actions) { const actions = el("col"); actions.style.width = "40px"; group.append(actions); }
  table.prepend(group);
  // The table's minimum width is set by apply(), after the first measure: a minimum would widen measured columns.
  const minTable = side + entries.reduce((sum, entry) => sum + entry.floor, 0);
  const pins = pinned.get(widths) ?? new Set<string>(); pinned.set(widths, pins);
  const state = measures.get(widths) ?? { specs: new Map<string, Spec>(), empty: false, room: 0 }; measures.set(widths, state);
  const complete = (): boolean => entries.every(({field}) => widths.has(field.name));
  const hasRecords = (): boolean => table.querySelector("tbody tr[data-id], tbody tr.x2many-row") !== null;
  const apply = (): void => {
    let total = side;
    for (const {field, col, controlsMin} of entries) {
      const saved = widths.get(field.name);
      if (saved === undefined) throw new Error(`Missing column width for ${field.name}.`);
      // Column choices may move the chooser to a header with a previously saved narrower width.
      const width = Math.max(saved, controlsMin);
      col.style.width = `${width}px`; total += width;
    }
    // One data column (the last flexible one the user has not resized) has no fixed width: when the list is wider
    // than the columns fixed layout hands the spare room to it alone, so a number or a check box never widens. With
    // every column fixed it would also stretch the 40 px selector column in proportion. When the user pinned all of
    // them the table is only as wide as its columns.
    const spare = entries.findLast(({field, flexible}) => !pins.has(field.name) && flexible) ?? entries.findLast(({field}) => !pins.has(field.name));
    spare?.col.style.removeProperty("width");
    table.style.minWidth = spare ? `max(${minTable}px, var(--list-table-min-width, 0px))` : `${minTable}px`;
    table.style.tableLayout = "fixed"; table.style.width = `${total}px`;
    flagWide(table);
  };
  /** Reads every automatic column's one-line content width (header included) under the content layout, in one task:
   *  the content layout is never painted. Pinned columns keep the width the user gave them. */
  const measureNow = (): void => {
    table.style.removeProperty("table-layout"); table.style.removeProperty("width"); table.style.removeProperty("min-width");
    for (const {col} of entries) col.style.removeProperty("width");
    table.classList.add("list-auto-layout");
    // Rounded up: a column one fraction of a pixel narrower than its widest value cuts or wraps that value.
    for (const {field, head, spec} of entries) if (!pins.has(field.name)) state.specs.set(field.name, spec(Math.ceil(head.getBoundingClientRect().width)));
    table.classList.remove("list-auto-layout");
    state.empty = !hasRecords();
  };
  /** Fits the automatic columns to the room the list has now (`fitColumns`); pinned columns, and widths given without a
   *  measure, stay as they are. */
  const refit = (): void => {
    const room = (table.parentElement?.clientWidth ?? 0) - side;
    state.room = room;
    const auto = ({field}: { field: ListColumn }): boolean => !pins.has(field.name) && state.specs.has(field.name);
    const specs = entries.map(entry => auto(entry) ? state.specs.get(entry.field.name)! : { width: widths.get(entry.field.name)!, min: widths.get(entry.field.name)!, max: widths.get(entry.field.name)! });
    const fitted = fitColumns(specs, room);
    entries.forEach((entry, i) => { if (auto(entry)) widths.set(entry.field.name, Math.min(4096, fitted[i]!)); });
    apply();
  };
  /** Forgets the automatic widths, so the next settle measures again; widths the user set stay. */
  const forget = (): void => { for (const {field} of entries) if (!pins.has(field.name)) widths.delete(field.name); };
  /** A manual resize starts from what is drawn: the spare-width column's stored width is not what the screen shows. */
  const claim = (name: string): void => {
    if (pins.has(name)) return;
    pins.add(name);
    const entry = entries.find(item => item.field.name === name)!;
    if (!entry.col.style.width) widths.set(name, entry.clamp(entry.head.getBoundingClientRect().width));
  };
  const measure = (): void => {
    for (const {field, head, clamp} of entries) if (!widths.has(field.name)) widths.set(field.name, clamp(head.getBoundingClientRect().width));
  };
  // A content width is capped so one long text column cannot take the whole row. The columns are
  // measured again when a column is added (every automatic one, as the reference does) and when a list first measured
  // without records gets some; a resized list refits without measuring. Widths the user set are never replaced.
  // Measured only once the table is mounted: an unmounted table has no layout to measure.
  let pending = false;
  // A table on a hidden notebook page is attached but has no layout: the width watcher measures it once it is shown.
  const laidOut = (): boolean => table.isConnected && !table.closest("[hidden]");
  let late: MutationObserver | undefined, lateFrame = false;
  /** Measures once more, on the next frame, when cell content arrives after the first measure (LATE_CONTENT_MS). Only
   *  the rows are watched: the header's resize handles and the column widths are this function's own changes. The
   *  time of the first measure is kept with the widths, so a list redrawn meanwhile (a sub-list redraws on every
   *  change of its form) still follows its late content. */
  const followLateContent = (): void => {
    if (late || state.at === undefined || typeof MutationObserver !== "function") return;
    late = new MutationObserver(() => {
      if (state.at === undefined || performance.now() - state.at > LATE_CONTENT_MS) { late?.disconnect(); return; }
      if (lateFrame || pending) return;
      lateFrame = true;
      requestAnimationFrame(() => {
        lateFrame = false;
        if (!laidOut() || pending) return;
        forget(); pending = true; settleNow();
      });
    });
    for (const body of table.tBodies) late.observe(body, { subtree: true, childList: true, characterData: true });
  };
  const settleNow = (): boolean => {
    if (!pending) return true;
    if (!laidOut()) return false;
    pending = false;
    measureNow(); refit();
    state.at ??= performance.now(); followLateContent();
    return true;
  };
  const start = (): void => {
    pending = true;
    table.classList.add("list-auto-layout");
  };
  // A list redrawn with records after it was measured empty (a sub-list's first line) measures them.
  if (complete() && !(state.empty && hasRecords())) { apply(); followLateContent(); }
  else { forget(); start(); }
  // An owner that attaches the table later (a sub-list in a form) gets the measure and the width watcher on a frame.
  let watched: Element | undefined;
  const follow = (frames: number): void => {
    requestAnimationFrame(() => {
      if (table.parentElement && watched !== table.parentElement) watch();
      if ((!settleNow() || !watched) && frames < AUTO_LAYOUT_FRAMES) follow(frames + 1);
    });
  };
  for (const {field, head, col, manualMin, clamp} of entries) {
    const handle = el("span", undefined, "column-resize-handle");
    handle.tabIndex = 0; handle.setAttribute("role", "separator");
    handle.setAttribute("aria-label", `Resize ${field.label} column`);
    handle.setAttribute("aria-orientation", "vertical");
    handle.setAttribute("aria-valuemin", String(manualMin)); handle.setAttribute("aria-valuemax", "4096");
    handle.title = "Drag to resize, or use Left and Right arrow keys. Double-click or press Enter for the automatic width.";
    // The spare-width column has no inline width: what is drawn is what a user hears.
    const announce = (): void => handle.setAttribute("aria-valuenow", String(!col.style.width || !widths.has(field.name) ? clamp(head.getBoundingClientRect().width) : widths.get(field.name)));
    announce();
    handle.addEventListener("focus", announce);
    // A column is pinned only once a drag moves it: a click, or the double-click back to automatic, pins nothing.
    let drag: { pointer: number; x: number; width?: number } | undefined;
    handle.addEventListener("pointerdown", event => {
      if (event.button !== 0) return;
      event.preventDefault(); event.stopPropagation();
      drag = {pointer:event.pointerId,x:event.clientX};
      handle.setPointerCapture?.(event.pointerId); handle.focus();
    });
    handle.addEventListener("pointermove", event => {
      if (!drag || drag.pointer !== event.pointerId) return;
      if (drag.width === undefined) {
        if (Math.abs(event.clientX - drag.x) < 1) return;
        measure(); claim(field.name); drag.width = widths.get(field.name)!;
      }
      widths.set(field.name, clamp(drag.width + event.clientX - drag.x)); apply(); announce();
    });
    const stop = (): void => { drag = undefined; };
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
    handle.addEventListener("lostpointercapture", stop);
    // Back to the automatic width (the reference has no way back short of a window resize, which drops every width).
    const automatic = (): void => {
      if (!table.isConnected) return;
      pins.delete(field.name); measure(); measureNow(); refit(); announce();
    };
    handle.addEventListener("dblclick", event => { event.preventDefault(); event.stopPropagation(); automatic(); });
    handle.addEventListener("keydown", event => {
      if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); automatic(); return; }
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault(); event.stopPropagation(); measure(); claim(field.name);
      widths.set(field.name, clamp(widths.get(field.name)! + (event.key === "ArrowRight" ? 10 : -10)));
      apply(); announce();
    });
    head.append(handle);
  }
  /** Refits when the list's width changes (window, side panel, scroll bar), keeping what the user set; the reference
   *  drops every width on a window resize. Also keeps the sideways-scroll flag current. */
  const watch = (): void => {
    const box = table.parentElement;
    if (!box || typeof ResizeObserver !== "function") { watched = box ?? undefined; flagWide(table); return; }
    wideWatchers.get(box)?.disconnect();
    const watcher = new ResizeObserver(() => {
      if (!laidOut()) return;
      if (pending) { settleNow(); return; }
      if (complete() && Math.abs(box.clientWidth - side - state.room) >= 1) refit();
      else flagWide(table);
    });
    watcher.observe(box); wideWatchers.set(box, watcher); watched = box; flagWide(table);
  };
  follow(0);
  return () => {
    // A list measured without records now has some (a filter removed, a group opened): measure them.
    if (!pending && state.empty && table.isConnected && hasRecords()) { forget(); pending = true; }
    settleNow(); watch();
  };
}

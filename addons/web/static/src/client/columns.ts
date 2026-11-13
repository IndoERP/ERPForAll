/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";

/** Widest content-sized column on first draw, before the columns are scaled to fit the list. */
const AUTO_COLUMN_MAX = 480;
/** Frames to wait for an off-screen list to be mounted before giving up on content sizing (about 2 s at 60 Hz). */
const AUTO_LAYOUT_FRAMES = 120;

/** Columns the user resized by hand, per widths map (so they survive a redraw): they keep a fixed width, the spare room
 *  goes to the last column that was not. When every column is pinned the table is as wide as its columns. */
const pinned = new WeakMap<Map<string, number>, Set<string>>();
const wideWatchers = new WeakMap<Element, ResizeObserver>();
/** A table wider than its list (columns widened by hand) scrolls sideways; otherwise the list does not scroll, so the
 *  header can stick to the page (client.css). The slack covers the rounding of the fitted widths, one pixel per column. */
function flagWide(table: HTMLTableElement): void {
  const box = table.parentElement;
  box?.toggleAttribute("data-wide", table.offsetWidth > (box?.clientWidth ?? 0) + table.querySelectorAll("col").length);
}
function watchWide(table: HTMLTableElement): void {
  const box = table.parentElement;
  if (!box || typeof ResizeObserver !== "function") return;
  wideWatchers.get(box)?.disconnect();
  const watcher = new ResizeObserver(() => flagWide(table));
  watcher.observe(box); wideWatchers.set(box, watcher); flagWide(table);
}

/** Pointer capture keeps resize listeners inside the table's own lifecycle. Widths live only in this list view. */
/** Returns `settle`: the owner calls it right after attaching the table, so the first-draw content sizing runs in the
 *  same task and the content layout is never painted. A table attached later settles on an animation frame. */
export function resizeColumns(table: HTMLTableElement, columns: readonly { name: string; label: string }[], widths: Map<string, number>): () => void {
  const row = table.tHead?.rows[0];
  if (!row) throw new Error("Resizable columns require a table header.");
  const group = el("colgroup"), selector = el("col");
  selector.style.width = "40px";
  group.append(selector);
  const entries = columns.map((field, index) => {
    const head = row.cells[index + 1];
    if (!head) throw new Error(`Missing header for ${field.name}.`);
    const col = el("col"); group.append(col);
    return { field, head, col };
  });
  table.prepend(group);
  // Optional appearances can fill a widening viewport without changing stored column widths.
  table.style.minWidth = `max(${40 + columns.length * 80}px, var(--list-table-min-width, 0px))`;
  const clamp = (value: number): number => Math.max(80, Math.min(4096, Math.round(value)));
  const pins = pinned.get(widths) ?? new Set<string>(); pinned.set(widths, pins);
  const apply = (): void => {
    let total = 40;
    for (const {field, col} of entries) {
      const width = widths.get(field.name);
      if (width === undefined) throw new Error(`Missing column width for ${field.name}.`);
      col.style.width = `${width}px`; total += width;
    }
    // One data column (the last one the user has not resized) has no fixed width: when the list is wider than the
    // columns (a window widened after the widths were fixed) fixed layout hands the spare room to it alone. With every
    // column fixed it would also stretch the 40 px selector column in proportion. When the user pinned all of them
    // the table is only as wide as its columns.
    const spare = entries.findLast(({field}) => !pins.has(field.name));
    spare?.col.style.removeProperty("width");
    table.style.minWidth = spare ? `max(${40 + columns.length * 80}px, var(--list-table-min-width, 0px))` : `${40 + columns.length * 80}px`;
    table.style.tableLayout = "fixed"; table.style.width = `${total}px`;
    flagWide(table);
  };
  /** A manual resize starts from what is drawn: the spare-width column's stored width is not what the screen shows. */
  const claim = (name: string, head: HTMLElement): void => {
    if (pins.has(name)) return;
    pins.add(name);
    const col = entries.find(entry => entry.field.name === name)!.col;
    if (!col.style.width) widths.set(name, clamp(head.getBoundingClientRect().width));
  };
  const measure = (): void => {
    for (const {field, head} of entries) if (!widths.has(field.name)) widths.set(field.name, clamp(head.getBoundingClientRect().width));
  };
  // A content width is capped so one long text column cannot take the whole row. Widths the user set by
  // resizing are never replaced. Measured while the content layout is in force (the class comes off only after), and
  // only once the table is mounted: an unmounted table has no layout to measure.
  let pending = false;
  const settleNow = (): boolean => {
    if (!pending) return true;
    if (entries.every(({field}) => widths.has(field.name))) { pending = false; table.classList.remove("list-auto-layout"); apply(); return true; }
    if (!table.isConnected) return false;
    const content = entries.map(({field, head}) => ({ name: field.name, width: Math.min(AUTO_COLUMN_MAX, clamp(head.getBoundingClientRect().width)) }));
    pending = false;
    table.classList.remove("list-auto-layout");
    const used = content.reduce((sum, column) => sum + column.width, 0);
    const available = (table.parentElement?.clientWidth ?? 0) - 40;
    // Fill a wider container by scaling up; in a narrower one (a phone, many columns) shrink the part above the
    // 80 px minimum so the list fits instead of overflowing the screen. Below n × 80 px every column keeps 80 px.
    const floor = content.length * 80;
    const fitted = (width: number): number => used <= 0 ? width
      : available >= used ? width * available / used
      : available <= floor ? 80
      : 80 + (width - 80) * (available - floor) / (used - floor);
    for (const column of content) if (!widths.has(column.name)) widths.set(column.name, clamp(fitted(column.width)));
    apply();
    return true;
  };
  if (entries.every(({field}) => widths.has(field.name))) apply();
  else {
    pending = true;
    table.classList.add("list-auto-layout");
    const settle = (frames: number): void => { requestAnimationFrame(() => { if (!settleNow() && frames < AUTO_LAYOUT_FRAMES) settle(frames + 1); }); };
    settle(0);
  }
  for (const {field, head} of entries) {
    const handle = el("span", undefined, "column-resize-handle");
    handle.tabIndex = 0; handle.setAttribute("role", "separator");
    handle.setAttribute("aria-label", `Resize ${field.label} column`);
    handle.setAttribute("aria-orientation", "vertical");
    handle.setAttribute("aria-valuemin", "80"); handle.setAttribute("aria-valuemax", "4096");
    handle.title = "Drag to resize, or use Left and Right arrow keys";
    const col = entries.find(entry => entry.field === field)!.col;
    // The spare-width column has no inline width: what is drawn is what a user hears.
    const announce = (): void => handle.setAttribute("aria-valuenow", String(!col.style.width || !widths.has(field.name) ? clamp(head.getBoundingClientRect().width) : widths.get(field.name)));
    announce();
    handle.addEventListener("focus", announce);
    let drag: { pointer: number; x: number; width: number } | undefined;
    handle.addEventListener("pointerdown", event => {
      if (event.button !== 0) return;
      event.preventDefault(); event.stopPropagation(); measure(); claim(field.name, head);
      drag = {pointer:event.pointerId,x:event.clientX,width:widths.get(field.name)!};
      handle.setPointerCapture?.(event.pointerId); handle.focus();
    });
    handle.addEventListener("pointermove", event => {
      if (!drag || drag.pointer !== event.pointerId) return;
      widths.set(field.name, clamp(drag.width + event.clientX - drag.x)); apply(); announce();
    });
    const stop = (): void => { drag = undefined; };
    handle.addEventListener("pointerup", stop);
    handle.addEventListener("pointercancel", stop);
    handle.addEventListener("lostpointercapture", stop);
    handle.addEventListener("keydown", event => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      event.preventDefault(); event.stopPropagation(); measure(); claim(field.name, head);
      widths.set(field.name, clamp(widths.get(field.name)! + (event.key === "ArrowRight" ? 10 : -10)));
      apply(); announce();
    });
    head.append(handle);
  }
  return () => { settleNow(); watchWide(table); };
}

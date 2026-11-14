/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The pivot view's DOM: toolbar, the table with its header buttons, the expand menu and the
// keyboard. The model (`pivot.ts`) owns every read; this file only draws its state and asks it for changes.
import { icon } from "./icons.js";
import { formatNumeric, type NumericLocale } from "./numeric-values.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { PIVOT_INTERVALS, groupingKey, pivotExportDocument, type PivotExportDocument, type PivotField, type PivotInterval, type PivotMeasure, type PivotModel, type PivotTable } from "./pivot.js";

const el = <K extends keyof HTMLElementTagNameMap>(tag: K, text?: string, className?: string): HTMLElementTagNameMap[K] => {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
};
const button = (text: string, className: string, onClick: () => void): HTMLButtonElement => {
  const node = el("button", text, className);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
};
const INTERVAL_LABELS: Record<PivotInterval, string> = { day: "Day", week: "Week", month: "Month", quarter: "Quarter", year: "Year" };

/** What a click on a value cell asks the application to open: the records behind it. */
export interface PivotCellTarget { domain: unknown[]; rowLabels: string[]; colLabels: string[] }
export interface PivotView {
  element: HTMLElement;
  /** An empty container at the end of the toolbar for buttons other slices add (the download). */
  toolbarEnd: HTMLElement;
  /** Draw the model's present state again. */
  render(): void;
  /** Stops every pending job of this view from drawing or changing the model. */
  dispose(): void;
}
export interface PivotViewOptions {
  model: PivotModel;
  locale: NumericLocale;
  title: string;
  /** The application's job runner: it reports a failed read. The model queues its own jobs, so two actions in flight end in the state of the last one. */
  run: (job: () => Promise<void>) => void;
  /** Open the records of a cell. Absent: cells are not links (the arch's `disable_linking` does the same). */
  openCell?: (target: PivotCellTarget) => Promise<void>;
  /** Called after every change the user made, once the table is drawn (the application keeps the address current). */
  changed?: () => void;
  /** The Download button posts the table on screen; it is absent when the host gives no `download`. */
  download?: { model: string; send: (document: PivotExportDocument) => Promise<void> };
}

export function mountPivot(options: PivotViewOptions): PivotView {
  const { model, locale, title, run } = options;
  const root = el("div", undefined, "pivot-view");
  const toolbar = el("div", undefined, "pivot-toolbar");
  toolbar.setAttribute("role", "toolbar"); toolbar.setAttribute("aria-label", "Main actions");
  const wrap = el("div", undefined, "nav-dropdown");
  const trigger = el("button", undefined, "nav-dropdown-toggle pivot-measures-toggle");
  trigger.type = "button";
  trigger.append(el("span", "Measures"), icon("chevron-down"));
  const menu = el("div", undefined, "nav-dropdown-menu pivot-measures-menu");
  wrap.append(trigger, menu);
  const act = (job: () => Promise<void>, focus?: string): void => run(async () => {
    try { await job(); } finally { render(); }
    if (disposed) return; // the user left the pivot while the job was in flight
    if (focus) restoreFocus(focus);
    options.changed?.();
  });
  const flip = button("Flip axis", "pivot-tool", () => act(() => model.flip()));
  const expandAll = button("Expand all", "pivot-tool", () => act(() => model.expandAll()));
  const collapseAll = button("Collapse all", "pivot-tool", () => act(() => model.collapseAll()));
  const toolbarEnd = el("div", undefined, "pivot-toolbar-end");
  toolbar.append(wrap, flip, expandAll, collapseAll, toolbarEnd);
  let sending = false;
  const downloadButton = el("button", undefined, "pivot-tool pivot-download");
  downloadButton.type = "button";
  downloadButton.setAttribute("aria-label", "Download xlsx");
  downloadButton.append(icon("file-text"), el("span", "Download"));
  if (options.download) {
    toolbarEnd.append(downloadButton);
    downloadButton.addEventListener("click", () => run(async () => {
      if (!options.download || sending) return;
      const posted = pivotExportDocument(model, { model: options.download.model, title });
      sending = true; render();
      try { await options.download.send(posted); } finally { sending = false; render(); }
    }));
  }
  const scroller = el("div", undefined, "table-scroll pivot-scroll");
  const status = el("p", undefined, "pivot-status muted");
  status.setAttribute("role", "status");
  const fieldMenu = el("div", undefined, "nav-dropdown-menu pivot-field-menu");
  fieldMenu.setAttribute("role", "menu"); fieldMenu.setAttribute("aria-label", "Expand by");
  root.append(toolbar, scroller, status, fieldMenu);
  const dropdown = attachMenuDropdown({ trigger, menu, mode: "class-open", items: () => [...menu.querySelectorAll<HTMLElement>("[role=menuitemcheckbox]")] });

  // A zero is shown for a group that has records whose measure holds no value; a group with no records is left blank.
  const format = (measure: PivotMeasure, value: number | null | undefined): string =>
    value === undefined ? "" : formatNumeric(value ?? 0, measure.kind, locale, measure.kind === "float" ? 2 : undefined);

  const drawMenu = (): void => {
    menu.replaceChildren(...model.config.measures.map(measure => {
      const on = model.active.includes(measure.id);
      const item = el("button", undefined, "dropdown-item pivot-measure-item");
      item.type = "button";
      const check = el("span", undefined, "pivot-check");
      if (on) check.append(icon("square-check-big"));
      item.append(check, el("span", measure.label));
      item.setAttribute("role", "menuitemcheckbox"); item.setAttribute("aria-checked", String(on));
      item.dataset["measure"] = measure.id;
      item.addEventListener("click", () => run(async () => {
        try { await model.toggleMeasure(measure.id); } finally { render(); }
        if (disposed) return;
        // The menu is drawn again: the item that was pressed takes focus back.
        menu.querySelector<HTMLElement>(`[data-measure="${measure.id}"]`)?.focus();
        options.changed?.();
      }));
      return item;
    }));
    dropdown.refresh();
  };

  // ---- the expand menu: the search view's group-by items, then every field the server can group by ----
  let menuFor: { key: string; trigger: HTMLElement } | null = null;
  const closeFieldMenu = (restore: boolean): void => {
    if (!menuFor) return;
    const { key } = menuFor;
    menuFor = null;
    fieldMenu.classList.remove("open"); fieldMenu.replaceChildren();
    if (restore) restoreFocus(key, true);
  };
  const menuItems = (): HTMLElement[] => [...fieldMenu.querySelectorAll<HTMLElement>("[role=menuitem]")].filter(item => item.offsetParent !== null || item.closest("[hidden]") === null);
  const openFieldMenu = (axis: "row" | "col", path: string[], anchor: HTMLElement, key: string): void => {
    closeFieldMenu(false);
    const choose = (field: PivotField): void => { closeFieldMenu(false); act(() => model.expand(axis, path, field), key); };
    const entries: HTMLElement[] = [];
    // A heading is not an entry: a menu owns only its items, separators and groups.
    const heading = (text: string): HTMLElement => { const node = el("div", text, "pivot-menu-heading"); node.setAttribute("role", "presentation"); return node; };
    const item = (label: string, onClick: () => void, className = ""): HTMLButtonElement => {
      const node = button(label, `dropdown-item pivot-field-item ${className}`.trim(), onClick);
      node.setAttribute("role", "menuitem");
      return node;
    };
    // What already groups this axis is not offered for it again; the model refuses it as well.
    const used = new Set(Array.from({ length: model.levelCount[axis] }, (_, depth) => groupingKey(model.levelAt(axis, depth)!)));
    const free = (field: PivotField): boolean => !used.has(groupingKey(field));
    const declared = model.config.items.filter(found => free(found.field));
    if (declared.length) {
      entries.push(heading("Group by"));
      for (const found of declared) entries.push(item(found.description, () => choose(found.field)));
      const separator = el("div", undefined, "pivot-menu-separator");
      separator.setAttribute("role", "separator");
      entries.push(separator);
    }
    entries.push(heading("Fields"));
    for (const field of [...model.config.groupable.values()].sort((a, b) => a.label.localeCompare(b.label))) {
      if (field.kind !== "date" && field.kind !== "datetime") { if (free(field)) entries.push(item(field.label, () => choose(field))); continue; }
      const intervals = [...PIVOT_INTERVALS].reverse().filter(interval => free({ ...field, interval }));
      if (!intervals.length) continue;
      const parent = item(field.label, () => undefined, "pivot-field-parent");
      parent.setAttribute("aria-haspopup", "menu"); parent.setAttribute("aria-expanded", "false");
      parent.append(icon("chevron-down"));
      const group = el("div", undefined, "pivot-interval-group");
      group.setAttribute("role", "group"); group.setAttribute("aria-label", `${field.label} by`); group.hidden = true;
      for (const interval of intervals) group.append(item(INTERVAL_LABELS[interval], () => choose({ ...field, interval }), "pivot-interval-item"));
      const toggle = (open: boolean): void => { group.hidden = !open; parent.setAttribute("aria-expanded", String(open)); };
      parent.addEventListener("click", () => toggle(Boolean(group.hidden)));
      parent.addEventListener("keydown", event => {
        if (event.key === "ArrowRight") { event.preventDefault(); toggle(true); group.querySelector<HTMLElement>("[role=menuitem]")?.focus(); }
        if (event.key === "ArrowLeft") { event.preventDefault(); toggle(false); }
      });
      entries.push(parent, group);
    }
    if (!entries.some(entry => entry.getAttribute("role") === "menuitem")) {
      // Every groupable field already groups this axis: say so in an entry that takes the focus, so Escape still closes the menu.
      const none = item(`No other field to group these ${axis === "row" ? "rows" : "columns"} by`, () => closeFieldMenu(true), "pivot-field-none");
      none.setAttribute("aria-disabled", "true");
      entries.length = 0; // no heading over nothing
      entries.push(none);
    }
    fieldMenu.replaceChildren(...entries);
    const rect = anchor.getBoundingClientRect();
    fieldMenu.style.insetInlineStart = `${Math.max(8, Math.min(rect.left, window.innerWidth - 232))}px`;
    fieldMenu.style.top = `${rect.bottom + 4}px`;
    fieldMenu.style.maxHeight = `${Math.max(160, window.innerHeight - rect.bottom - 16)}px`;
    fieldMenu.classList.add("open");
    menuFor = { key, trigger: anchor };
    menuItems()[0]?.focus();
  };
  fieldMenu.addEventListener("keydown", event => {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); closeFieldMenu(true); return; }
    if (event.key === "Tab") { closeFieldMenu(false); return; }
    const items = menuItems(), at = items.indexOf(document.activeElement as HTMLElement);
    const to = event.key === "ArrowDown" ? (at + 1) % items.length : event.key === "ArrowUp" ? (at - 1 + items.length) % items.length : event.key === "Home" ? 0 : event.key === "End" ? items.length - 1 : -1;
    if (to >= 0) { event.preventDefault(); items[to]?.focus(); }
  });
  const onPointerDown = (event: PointerEvent): void => { if (menuFor && !fieldMenu.contains(event.target as Node) && !menuFor.trigger.contains(event.target as Node)) closeFieldMenu(false); };
  document.addEventListener("pointerdown", onPointerDown);

  // ---- the table ----
  const headerKey = (axis: "row" | "col", path: readonly string[]): string => `${axis}:${path.join("\u0000")}`;
  /** A redraw removes the header that was pressed: it takes focus again, unless the user has moved on. */
  const restoreFocus = (key: string, force = false): void => {
    if (!force && document.activeElement && document.activeElement !== document.body) return;
    [...root.querySelectorAll<HTMLElement>("[data-pivot-header]")].find(node => node.dataset["pivotHeader"] === key)?.focus();
  };

  /** A header is a node (reference `pivot_renderer.xml`): the icon says closed or opened, the whole cell is the target, the button inside is what the keyboard and a screen reader use. */
  const headerButton = (axis: "row" | "col", header: { path: string[]; label: string; open: boolean }, cell: HTMLElement): HTMLElement => {
    const key = headerKey(axis, header.path);
    const next = model.levelAt(axis, header.path.length);
    const canExpand = next !== undefined || model.config.groupable.size > 0;
    const label = header.path.length ? header.label : "Total";
    if (!header.open && !canExpand) return el("span", label);
    const node = el("button", undefined, "pivot-toggle");
    node.type = "button";
    node.dataset["pivotHeader"] = key;
    node.setAttribute("aria-expanded", String(header.open));
    // The accessible name starts with the visible word (WCAG 2.5.3); a Total says which axis it totals.
    if (!header.path.length) node.setAttribute("aria-label", `Total, ${axis === "row" ? "rows" : "columns"}${header.open && next ? ` grouped by ${next.label}` : ""}`);
    const needsMenu = !header.open && next === undefined;
    if (needsMenu) node.setAttribute("aria-haspopup", "menu");
    node.title = label;
    // The depth is only padding for the eye: say it in words too.
    if (header.path.length) node.setAttribute("aria-description", `${axis === "row" ? "Row" : "Column"} level ${header.path.length}`);
    node.append(icon(header.open ? "square-minus" : "square-plus"), el("span", label, "pivot-toggle-label"));
    cell.classList.add("pivot-node", header.open ? "pivot-node-opened" : "pivot-node-closed");
    cell.addEventListener("click", event => { if (!node.contains(event.target as Node)) node.click(); });
    const activate = (): void => {
      if (header.open) act(() => model.collapse(axis, header.path), key);
      else if (next) act(() => model.expand(axis, header.path), key);
      else openFieldMenu(axis, header.path, node, key);
    };
    node.addEventListener("click", activate);
    node.addEventListener("keydown", event => {
      // The key states the wanted state; the model decides from its own state, which a pending read has not drawn yet.
      if (event.key === "ArrowRight" && !header.open) { event.preventDefault(); if (next) act(() => model.setOpen(axis, header.path, true), key); else openFieldMenu(axis, header.path, node, key); }
      else if (event.key === "ArrowLeft" && header.open) { event.preventDefault(); act(() => model.setOpen(axis, header.path, false), key); }
    });
    return node;
  };

  const th = (className: string, scope: "row" | "col", span?: { colSpan?: number; rowSpan?: number }): HTMLTableCellElement => {
    const cell = el("th", undefined, className);
    cell.scope = scope;
    // A header over several columns is a column group: with scope "col" a screen reader names only the first column under it.
    if (span?.colSpan && span.colSpan > 1) { cell.colSpan = span.colSpan; if (scope === "col") cell.scope = "colgroup"; }
    if (span?.rowSpan && span.rowSpan > 1) cell.rowSpan = span.rowSpan;
    return cell;
  };

  let disposed = false;
  const drawTable = (table: PivotTable): HTMLTableElement => {
    const m = table.measures.length;
    const { rows, headerRows, leaves, totalColumn } = table;
    const element = el("table", undefined, "pivot-table");
    const rowLevels = Array.from({ length: model.levelCount.row }, (_, depth) => model.levelAt("row", depth)!.label);
    const colLevels = Array.from({ length: model.levelCount.col }, (_, depth) => model.levelAt("col", depth)!.label);
    element.append(el("caption", `${title}${rowLevels.length ? `, rows by ${rowLevels.join(", then ")}` : ""}${colLevels.length ? `, columns by ${colLevels.join(", then ")}` : ""}`, "visually-hidden"));
    const head = el("thead");
    const corner = th("pivot-corner", "col", { rowSpan: headerRows.length + 1 });
    corner.textContent = rowLevels.length ? rowLevels.join(" / ") : "Records";
    headerRows.forEach((cells, depth) => {
      const tr = el("tr", undefined, depth === 0 ? "pivot-head-top" : "pivot-head-groups");
      if (depth === 0) tr.append(corner);
      for (const header of cells) {
        const cell = th(`pivot-col-header${header.path.length ? "" : " pivot-total"}`, "col", { colSpan: header.span * m, rowSpan: header.rowSpan });
        cell.append(headerButton("col", header, cell));
        tr.append(cell);
      }
      if (depth === 0 && totalColumn) {
        const total = th("pivot-col-header pivot-total", "col", { colSpan: m, rowSpan: headerRows.length });
        total.textContent = "Total";
        tr.append(total);
      }
      head.append(tr);
    });
    const measureRow = el("tr", undefined, "pivot-head-measures");
    const columns: string[][] = totalColumn ? [...leaves, []] : leaves;
    columns.forEach((path, index) => {
      for (const measure of table.measures) {
        const here = table.sort?.measure === measure.id && table.sort.path.join("\u0000") === path.join("\u0000");
        const cell = th(`pivot-measure-header${totalColumn && index === columns.length - 1 ? " pivot-total" : ""}`, "col");
        cell.setAttribute("aria-sort", here ? (table.sort!.desc ? "descending" : "ascending") : "none");
        const sort = el("button", undefined, "pivot-sort");
        sort.type = "button";
        sort.append(el("span", measure.label));
        if (here) sort.append(icon("chevron-down"));
        sort.classList.toggle("pivot-sort-asc", here && !table.sort!.desc);
        sort.setAttribute("aria-label", `${measure.label}, sort rows${here ? (table.sort!.desc ? ", sorted descending, press to clear" : ", sorted ascending, press for descending") : ""}`);
        sort.addEventListener("click", () => act(() => model.sortBy(measure.id, path)));
        cell.append(sort);
        measureRow.append(cell);
      }
    });
    head.append(measureRow);

    const body = el("tbody");
    const linking = options.openCell !== undefined && !model.config.disableLinking;
    const rowNames = new Map(rows.map(row => [row.path.join("\u0000"), row.label] as const));
    const colNames = new Map(headerRows.flat().map(header => [header.path.join("\u0000"), header.label] as const));
    const crumbs = (names: ReadonlyMap<string, string>, path: string[]): string[] => path.map((_, i) => names.get(path.slice(0, i + 1).join("\u0000")) ?? "");
    for (const row of rows) {
      const tr = el("tr", undefined, row.path.length ? "pivot-row" : "pivot-row pivot-row-total");
      const header = th(`pivot-row-header${row.path.length ? "" : " pivot-total"}`, "row");
      header.style.setProperty("--pivot-depth", String(row.depth));
      header.append(headerButton("row", row, header));
      tr.append(header);
      columns.forEach((col, index) => {
        const isTotalColumn = totalColumn && index === columns.length - 1;
        for (const measure of table.measures) {
          const value = table.cell(row.path, col, measure);
          const cell = el("td", undefined, `pivot-cell${!row.path.length || !col.length || isTotalColumn ? " pivot-total" : ""}${value === undefined ? " pivot-empty" : ""}`);
          const text = format(measure, value);
          const domain = linking && value !== undefined ? model.cellDomain(row.path, col) : undefined;
          if (domain) {
            const rowLabels = crumbs(rowNames, row.path), colLabels = crumbs(colNames, col);
            const link = el("button", text, "pivot-cell-link");
            link.type = "button"; link.tabIndex = -1;
            link.setAttribute("aria-label", `${text} ${measure.label}, ${[...rowLabels, ...colLabels].join(", ") || "Total"}: open the records`);
            link.addEventListener("click", () => run(async () => { await options.openCell!({ domain, rowLabels, colLabels }); }));
            cell.append(link);
          } else cell.textContent = text;
          tr.append(cell);
        }
      });
      body.append(tr);
    }
    element.append(head, body);
    // The value cells that open records are one tab stop; the arrow keys move between them.
    const grid = [...body.rows].map(row => [...row.cells].map(cell => cell.querySelector<HTMLButtonElement>(".pivot-cell-link")));
    const first = grid.flat().find(link => link);
    if (first) first.tabIndex = 0;
    element.addEventListener("keydown", event => {
      const target = event.target as HTMLElement;
      if (!target.classList.contains("pivot-cell-link")) return;
      const [dr, dc] = event.key === "ArrowDown" ? [1, 0] : event.key === "ArrowUp" ? [-1, 0] : event.key === "ArrowRight" ? [0, 1] : event.key === "ArrowLeft" ? [0, -1] : [0, 0];
      if (!dr && !dc) return;
      event.preventDefault();
      let r = grid.findIndex(line => line.includes(target as HTMLButtonElement));
      let c = r < 0 ? -1 : grid[r]!.indexOf(target as HTMLButtonElement);
      for (;;) {
        r += dr; c += dc;
        const line = grid[r];
        if (!line || c < 0 || c >= line.length) return;
        const next = line[c];
        if (next) { next.focus(); return; }
      }
    });
    // Whichever value cell has focus is the table's one tab stop.
    element.addEventListener("focusin", event => {
      const target = event.target as HTMLElement;
      if (!target.classList.contains("pivot-cell-link")) return;
      for (const link of grid.flat()) if (link) link.tabIndex = link === target ? 0 : -1;
    });
    return element;
  };

  const render = (): void => {
    if (disposed) return;
    closeFieldMenu(false);
    const table = model.table();
    drawMenu();
    // Nothing to flip, expand or fold when there is nothing to show (reference `pivot_controller.xml`).
    const counts = model.levelCount;
    flip.disabled = !table.hasData;
    expandAll.disabled = !table.hasData || counts.row + counts.col === 0;
    collapseAll.disabled = !table.hasData || (!table.rows.some(row => row.open) && !table.headerRows.some(cells => cells.some(header => header.open)));
    status.textContent = "";
    const reason = table.unavailable ? "The table could not be loaded, so there is nothing to download." : !table.measures.length ? "No measure is selected." : !table.hasData ? "There is nothing to show: no record matches the search." : sending ? "The download is being prepared." : "";
    downloadButton.disabled = reason !== "";
    if (reason) downloadButton.title = reason; else downloadButton.removeAttribute("title");
    if (table.unavailable) { scroller.replaceChildren(); status.textContent = "Unable to load the table. Adjust the search or reload the page."; return; }
    if (!table.measures.length) { scroller.replaceChildren(); status.textContent = "No measure is selected. Choose one in the Measures menu."; return; }
    if (!table.hasData) { scroller.replaceChildren(); status.textContent = "There is nothing to show: no record matches the search."; return; }
    scroller.replaceChildren(drawTable(table));
  };

  render();
  return {
    element: root, toolbarEnd, render,
    dispose: () => { disposed = true; model.dispose(); dropdown.dispose(); document.removeEventListener("pointerdown", onPointerDown); }
  };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { csvCell } from "./csv.js";
import { icon } from "@erpforall/client";

export type ColumnItem =
  | string
  | {
      key?: string;
      label?: string;
      name?: string;
      title?: string;
      field?: string;
      dataIndex?: string;
    };

export type TableSpec = {
  allowExport?: boolean;
  title?: string;
  columns?: ColumnItem[];
  rows?: unknown[];
  records?: Record<string, unknown>[];
};

interface NormalizedColumn {
  key: string;
  label: string;
}

export function renderRichTable(spec: TableSpec): HTMLElement {
  const container = document.createElement("div");
  container.className = "canvas-table-container";

  const rawInputRows: unknown[] = Array.isArray(spec.rows)
    ? spec.rows
    : Array.isArray(spec.records)
    ? spec.records
    : [];

  // Normalize column definitions from strings, objects, or keys
  let normalizedCols: NormalizedColumn[] = [];
  if (Array.isArray(spec.columns) && spec.columns.length > 0) {
    normalizedCols = spec.columns.map((col, idx) => {
      if (typeof col === "string") {
        return { key: col, label: col };
      }
      if (typeof col === "object" && col !== null) {
        const obj = col as Record<string, unknown>;
        const key = String(
          obj["key"] ??
            obj["name"] ??
            obj["field"] ??
            obj["dataIndex"] ??
            obj["id"] ??
            obj["label"] ??
            `col_${idx}`
        );
        const label = String(
          obj["label"] ??
            obj["title"] ??
            obj["header"] ??
            obj["name"] ??
            obj["key"] ??
            `Col ${idx + 1}`
        );
        return { key, label };
      }
      return { key: String(col), label: String(col) };
    });
  }

  // If columns are empty or not specified, derive from the first row object's keys
  if (!normalizedCols.length && rawInputRows.length > 0) {
    const first = rawInputRows[0];
    if (typeof first === "object" && first !== null && !Array.isArray(first)) {
      normalizedCols = Object.keys(first).map(k => ({ key: k, label: k }));
    }
  }

  if (!normalizedCols.length) {
    container.textContent = "Tidak ada kolom yang didefinisikan.";
    return container;
  }

  // Convert every row into a guaranteed (string | number | boolean | null)[]
  const rawRows: (string | number | boolean | null)[][] = rawInputRows.map(row => {
    if (Array.isArray(row)) {
      return row.map(cell => (cell === undefined ? null : (cell as any)));
    }
    if (typeof row === "object" && row !== null) {
      const rec = row as Record<string, unknown>;
      return normalizedCols.map(col => {
        const k = col.key;
        const l = col.label;
        const kLow = k.toLowerCase();
        const lLow = l.toLowerCase();
        const val =
          rec[k] ??
          rec[l] ??
          rec[kLow] ??
          rec[lLow] ??
          rec[k.replace(/ /g, "_").toLowerCase()] ??
          rec[l.replace(/ /g, "_").toLowerCase()];
        return val === undefined ? null : (val as any);
      });
    }
    return [row as any];
  });

  let sortColIndex = -1;
  let sortAsc = true;
  let filterText = "";
  let displayRows = [...rawRows];

  // Toolbar (Filter & CSV Export)
  const toolbar = document.createElement("div");
  toolbar.className = "canvas-table-toolbar";

  const searchBox = document.createElement("input");
  searchBox.type = "search";
  searchBox.placeholder = "Cari di tabel…";
  searchBox.className = "canvas-table-search";

  const exportBtn = document.createElement("button");
  exportBtn.type = "button";
  exportBtn.className = "erp-button erp-button-secondary canvas-table-export";
  exportBtn.textContent = "Ekspor CSV";

  const countBadge = document.createElement("span");
  countBadge.className = "canvas-table-count";
  countBadge.textContent = `${displayRows.length} baris`;

  toolbar.append(searchBox);
  if (spec.allowExport !== false) toolbar.append(exportBtn);
  toolbar.append(countBadge);

  // Table element
  const scrollWrapper = document.createElement("div");
  scrollWrapper.className = "canvas-table-scroll";

  const table = document.createElement("table");
  table.className = "canvas-data-table";

  const thead = table.createTHead();
  const headerRow = thead.insertRow();

  normalizedCols.forEach((col, idx) => {
    const th = document.createElement("th");
    th.scope = "col";
    th.className = "canvas-th-sortable";
    th.setAttribute("aria-sort", "none");
    th.tabIndex = 0;

    const labelSpan = document.createElement("span");
    labelSpan.textContent = col.label;

    const sortIndicator = document.createElement("span");
    sortIndicator.className = "canvas-sort-indicator";

    th.append(labelSpan, sortIndicator);

    const sortColumn = () => {
      if (sortColIndex === idx) {
        sortAsc = !sortAsc;
      } else {
        sortColIndex = idx;
        sortAsc = true;
      }
      renderRows();
    };
    th.addEventListener("click", sortColumn);
    th.addEventListener("keydown", event => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        sortColumn();
      }
    });

    headerRow.append(th);
  });

  const tbody = table.createTBody();

  function updateDisplayRows() {
    let rows = [...rawRows];
    if (filterText.trim()) {
      const q = filterText.toLowerCase();
      rows = rows.filter(r =>
        Array.isArray(r) && r.some(cell => cell !== null && String(cell).toLowerCase().includes(q))
      );
    }
    if (sortColIndex >= 0) {
      rows.sort((a, b) => {
        const valA = Array.isArray(a) ? a[sortColIndex] : null;
        const valB = Array.isArray(b) ? b[sortColIndex] : null;
        if (valA === valB) return 0;
        if (valA === null || valA === undefined) return 1;
        if (valB === null || valB === undefined) return -1;
        if (typeof valA === "number" && typeof valB === "number") {
          return sortAsc ? valA - valB : valB - valA;
        }
        const strA = String(valA);
        const strB = String(valB);
        return sortAsc ? strA.localeCompare(strB) : strB.localeCompare(strA);
      });
    }
    displayRows = rows;
    countBadge.textContent = `${displayRows.length} baris`;
  }

  function renderRows() {
    updateDisplayRows();

    // Update header sort indicators
    headerRow.querySelectorAll("th").forEach((th, idx) => {
      const indicator = th.querySelector(".canvas-sort-indicator");
      if (idx === sortColIndex) {
        th.setAttribute("aria-sort", sortAsc ? "ascending" : "descending");
        th.classList.add("active-sort");
        indicator?.replaceChildren(icon("chevron-down"));
      } else {
        th.setAttribute("aria-sort", "none");
        th.classList.remove("active-sort");
        indicator?.replaceChildren();
      }
    });

    tbody.replaceChildren();
    if (!displayRows.length) {
      const emptyTr = tbody.insertRow();
      const emptyTd = emptyTr.insertCell();
      emptyTd.colSpan = normalizedCols.length;
      emptyTd.className = "canvas-td-empty";
      emptyTd.textContent = "Tidak ada baris yang sesuai pencarian.";
      return;
    }

    displayRows.forEach(row => {
      const tr = tbody.insertRow();
      const cells = Array.isArray(row) ? row : [row];
      cells.forEach(val => {
        const td = tr.insertCell();
        if (val === null || val === undefined) {
          td.textContent = "—";
          td.classList.add("cell-null");
        } else if (typeof val === "number") {
          td.textContent = new Intl.NumberFormat("id-ID").format(val);
          td.classList.add("cell-numeric");
        } else {
          td.textContent = String(val);
        }
      });
    });
  }

  searchBox.addEventListener("input", () => {
    filterText = searchBox.value;
    renderRows();
  });

  exportBtn.addEventListener("click", () => {
    const title = spec.title || "tabel-ekspor";
    const csvLines = [
      normalizedCols.map(c => csvCell(c.label)).join(","),
      ...displayRows.map(r =>
        (Array.isArray(r) ? r : [r]).map(csvCell).join(",")
      ),
    ];
    const blob = new Blob([csvLines.join("\n")], { type: "text/csv;charset=utf-8;" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `${title.toLowerCase().replace(/[^a-z0-9_-]/g, "_")}-${Date.now()}.csv`;
    a.click();
    URL.revokeObjectURL(url);
  });

  scrollWrapper.append(table);
  container.append(toolbar, scrollWrapper);
  renderRows();
  return container;
}

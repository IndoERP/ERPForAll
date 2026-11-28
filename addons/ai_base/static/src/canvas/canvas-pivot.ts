/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { csvCell } from "./csv.js";

export type PivotMeasure = {
  key: string;
  label: string;
  agg?: "sum" | "count" | "avg";
};

export type PivotSpec = {
  title?: string;
  dimensions?: string[];
  measures?: PivotMeasure[];
  records?: Record<string, unknown>[];
  columns?: string[];
  rows?: (string | number | boolean | null)[][];
};

export function renderPivotTable(spec: PivotSpec): HTMLElement {
  const container = document.createElement("div");
  container.className = "canvas-pivot-container";

  if (spec.title) {
    const titleEl = document.createElement("div");
    titleEl.className = "canvas-pivot-title";
    titleEl.textContent = spec.title;
    container.append(titleEl);
  }

  const { dimensions: rawDimensions, measures: rawMeasures, records, columns: rawColumns, rows: rawRows } = spec;

  let activeDimension = rawDimensions && Array.isArray(rawDimensions) && rawDimensions.length ? rawDimensions[0]! : "Kategori";
  const dimensions = rawDimensions && Array.isArray(rawDimensions) && rawDimensions.length ? [...rawDimensions] : ["Kategori"];
  
  // Resolve measures
  let measures: PivotMeasure[] = [];
  if (Array.isArray(rawMeasures) && rawMeasures.length) {
    measures = rawMeasures.map(m => ({
      key: m.key,
      label: m.label || m.key,
      agg: m.agg || "sum",
    }));
  } else if (Array.isArray(records) && records.length) {
    const first = records[0]!;
    for (const [k, v] of Object.entries(first)) {
      if (typeof v === "number") {
        measures.push({ key: k, label: k.replace(/_/g, " ").toUpperCase(), agg: "sum" });
      }
    }
    if (!measures.length) {
      measures.push({ key: "_count", label: "Jumlah", agg: "count" });
    }
  } else if (Array.isArray(rawColumns) && rawColumns.length > 1) {
    measures = rawColumns.slice(1).map(c => ({ key: c, label: c, agg: "sum" }));
  }

  let filterText = "";
  let sortCol = "";
  let sortAsc = false;
  const collapsedGroups = new Set<string>();

  // Toolbar
  const toolbar = document.createElement("div");
  toolbar.className = "canvas-pivot-toolbar";

  // Dimension switchers
  const dimGroup = document.createElement("div");
  dimGroup.className = "canvas-pivot-dim-group";
  const dimLabel = document.createElement("span");
  dimLabel.className = "canvas-pivot-dim-label";
  dimLabel.textContent = "Grup Baris:";
  dimGroup.append(dimLabel);

  const dimButtons: HTMLButtonElement[] = [];
  dimensions.forEach(dim => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "canvas-pivot-dim-btn" + (dim === activeDimension ? " active" : "");
    btn.textContent = dim;
    btn.addEventListener("click", () => {
      activeDimension = dim;
      dimButtons.forEach(b => b.classList.toggle("active", b.textContent === dim));
      render();
    });
    dimButtons.push(btn);
    dimGroup.append(btn);
  });

  const searchBox = document.createElement("input");
  searchBox.type = "search";
  searchBox.placeholder = "Filter pivot data…";
  searchBox.className = "canvas-table-search canvas-pivot-search";
  searchBox.addEventListener("input", () => {
    filterText = searchBox.value.trim().toLowerCase();
    render();
  });

  const exportBtn = document.createElement("button");
  exportBtn.type = "button";
  exportBtn.className = "erp-button erp-button-secondary canvas-table-export";
  exportBtn.textContent = "Ekspor CSV";
  exportBtn.addEventListener("click", () => exportToCsv());

  toolbar.append(dimGroup, searchBox, exportBtn);

  // Table viewport
  const scrollWrapper = document.createElement("div");
  scrollWrapper.className = "canvas-table-scroll canvas-pivot-scroll";

  const table = document.createElement("table");
  table.className = "canvas-data-table canvas-pivot-table";
  scrollWrapper.append(table);

  container.append(toolbar, scrollWrapper);

  function formatValue(val: number): string {
    return new Intl.NumberFormat("id-ID").format(Math.round(val * 100) / 100);
  }

  function resolveItemDimValue(item: Record<string, unknown>, dim: string): string {
    const raw = item[dim];
    if (raw === undefined || raw === null) return "(Tidak Ada)";
    if (Array.isArray(raw)) return String(raw[1] || raw[0] || "");
    return String(raw);
  }

  type GroupedData = {
    groupKey: string;
    items: Record<string, unknown>[];
    subtotals: Record<string, number>;
  };

  function computeGroups(): GroupedData[] {
    if (!Array.isArray(records)) return [];
    const map = new Map<string, Record<string, unknown>[]>();
    for (const rec of records) {
      const g = resolveItemDimValue(rec, activeDimension);
      if (filterText && !String(g).toLowerCase().includes(filterText)) {
        // Check if any measure value or other fields contain filterText
        const matchAny = Object.values(rec).some(v => String(v).toLowerCase().includes(filterText));
        if (!matchAny) continue;
      }
      const list = map.get(g) || [];
      list.push(rec);
      map.set(g, list);
    }

    const groups: GroupedData[] = [];
    for (const [groupKey, items] of map.entries()) {
      const subtotals: Record<string, number> = {};
      for (const m of measures) {
        if (m.agg === "count") {
          subtotals[m.key] = items.length;
        } else if (m.agg === "avg") {
          const sum = items.reduce((acc, it) => acc + (Number(it[m.key]) || 0), 0);
          subtotals[m.key] = items.length ? sum / items.length : 0;
        } else {
          // sum
          subtotals[m.key] = items.reduce((acc, it) => acc + (Number(it[m.key]) || 0), 0);
        }
      }
      groups.push({ groupKey, items, subtotals });
    }

    // Sort
    if (sortCol) {
      groups.sort((a, b) => {
        const valA = sortCol === "__group" ? a.groupKey : (a.subtotals[sortCol] ?? 0);
        const valB = sortCol === "__group" ? b.groupKey : (b.subtotals[sortCol] ?? 0);
        if (typeof valA === "number" && typeof valB === "number") {
          return sortAsc ? valA - valB : valB - valA;
        }
        return sortAsc ? String(valA).localeCompare(String(valB)) : String(valB).localeCompare(String(valA));
      });
    }

    return groups;
  }

  function exportToCsv() {
    const groups = computeGroups();
    const headers = [activeDimension, ...measures.map(m => m.label)];
    const csvLines = [headers.map(csvCell).join(",")];

    for (const g of groups) {
      const row = [g.groupKey, ...measures.map(m => g.subtotals[m.key] ?? 0)];
      csvLines.push(row.map(csvCell).join(","));
    }

    const blob = new Blob([csvLines.join("\n")], { type: "text/csv;charset=utf-8;" });
    const link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = `pivot-${String(activeDimension).toLowerCase()}-${Date.now()}.csv`;
    link.click();
    URL.revokeObjectURL(link.href);
  }

  function render() {
    table.replaceChildren();

    // Thead
    const thead = table.createTHead();
    const headerRow = thead.insertRow();

    // Dimension header
    const thDim = document.createElement("th");
    thDim.className = "canvas-th-sortable" + (sortCol === "__group" ? " active-sort" : "");
    const dimSpan = document.createElement("span");
    dimSpan.textContent = activeDimension;
    const dimSortSpan = document.createElement("span");
    dimSortSpan.className = "canvas-sort-indicator";
    dimSortSpan.textContent = sortCol === "__group" ? (sortAsc ? "▲" : "▼") : "";
    thDim.append(dimSpan, " ", dimSortSpan);
    thDim.addEventListener("click", () => {
      if (sortCol === "__group") {
        sortAsc = !sortAsc;
      } else {
        sortCol = "__group";
        sortAsc = true;
      }
      render();
    });
    headerRow.append(thDim);

    // Measure headers
    measures.forEach(m => {
      const th = document.createElement("th");
      th.className = "canvas-th-sortable cell-numeric" + (sortCol === m.key ? " active-sort" : "");
      const labelSpan = document.createElement("span");
      labelSpan.textContent = m.label;
      const measureSortSpan = document.createElement("span");
      measureSortSpan.className = "canvas-sort-indicator";
      measureSortSpan.textContent = sortCol === m.key ? (sortAsc ? "▲" : "▼") : "";
      th.append(labelSpan, " ", measureSortSpan);
      th.addEventListener("click", () => {
        if (sortCol === m.key) {
          sortAsc = !sortAsc;
        } else {
          sortCol = m.key;
          sortAsc = false; // default desc for measures
        }
        render();
      });
      headerRow.append(th);
    });

    const tbody = table.createTBody();

    // Fallback if raw rows provided directly
    if (!Array.isArray(records) && Array.isArray(rawRows)) {
      rawRows.forEach(row => {
        const tr = tbody.insertRow();
        const cells: unknown[] = Array.isArray(row)
          ? row
          : typeof row === "object" && row !== null
          ? Object.values(row)
          : [row];
        cells.forEach((val, idx) => {
          const td = tr.insertCell();
          if (idx === 0) {
            td.textContent = String(val ?? "—");
            td.className = "canvas-pivot-group-cell";
          } else if (typeof val === "number") {
            td.textContent = formatValue(val);
            td.className = "cell-numeric";
          } else {
            td.textContent = String(val ?? "—");
          }
        });
      });
      return;
    }

    const groups = computeGroups();
    if (!groups.length) {
      const emptyRow = tbody.insertRow();
      const td = emptyRow.insertCell();
      td.colSpan = measures.length + 1;
      td.className = "canvas-td-empty";
      td.textContent = "Tidak ada data yang cocok dengan kriteria pivot.";
      return;
    }

    // Grand totals accumulator
    const grandTotals: Record<string, number> = {};
    measures.forEach(m => {
      grandTotals[m.key] = 0;
    });

    groups.forEach(g => {
      // Accumulate
      measures.forEach(m => {
        grandTotals[m.key] = (grandTotals[m.key] || 0) + (g.subtotals[m.key] || 0);
      });

      const isCollapsed = collapsedGroups.has(g.groupKey);

      // Group header row
      const tr = tbody.insertRow();
      tr.className = "canvas-pivot-group-row";

      const tdKey = tr.insertCell();
      tdKey.className = "canvas-pivot-group-cell";

      const toggleBtn = document.createElement("button");
      toggleBtn.type = "button";
      toggleBtn.className = "canvas-pivot-toggle-btn";
      toggleBtn.textContent = isCollapsed ? "►" : "▼";
      toggleBtn.setAttribute("aria-label", isCollapsed ? "Buka grup" : "Tutup grup");
      toggleBtn.addEventListener("click", e => {
        e.stopPropagation();
        if (collapsedGroups.has(g.groupKey)) {
          collapsedGroups.delete(g.groupKey);
        } else {
          collapsedGroups.add(g.groupKey);
        }
        render();
      });

      const nameSpan = document.createElement("strong");
      nameSpan.className = "canvas-pivot-group-name";
      nameSpan.textContent = g.groupKey;

      const countBadge = document.createElement("span");
      countBadge.className = "canvas-pivot-item-count";
      countBadge.textContent = `(${g.items.length})`;

      tdKey.append(toggleBtn, nameSpan, countBadge);

      measures.forEach(m => {
        const td = tr.insertCell();
        td.className = "cell-numeric canvas-pivot-subtotal-cell";
        td.textContent = formatValue(g.subtotals[m.key] ?? 0);
      });

      // Child item rows if expanded
      if (!isCollapsed && g.items.length > 1) {
        g.items.forEach(item => {
          const itemRow = tbody.insertRow();
          itemRow.className = "canvas-pivot-item-row";

          const itemTd = itemRow.insertCell();
          itemTd.className = "canvas-pivot-child-cell";
          const subLabel = Object.entries(item).find(([k]) => k !== activeDimension && typeof item[k] === "string");
          const text = subLabel ? String(subLabel[1]) : "—";
          const lower = text.toLowerCase();
          if (["draft", "sale", "cancel", "done", "posted", "paid"].includes(lower)) {
            const badge = document.createElement("span");
            badge.className = `pivot-state-badge pivot-state-${lower}`;
            badge.textContent = text;
            itemTd.append(badge);
          } else {
            itemTd.textContent = text;
          }

          measures.forEach(m => {
            const td = itemRow.insertCell();
            td.className = "cell-numeric canvas-pivot-child-num";
            const val = Number(item[m.key]);
            td.textContent = Number.isFinite(val) ? formatValue(val) : "—";
          });
        });
      }
    });

    // Grand Total Footer
    const tfoot = table.createTFoot();
    const footRow = tfoot.insertRow();
    footRow.className = "canvas-pivot-grandtotal-row";

    const footLabel = footRow.insertCell();
    footLabel.className = "canvas-pivot-grandtotal-label";
    footLabel.textContent = "TOTAL KESELURUHAN";

    measures.forEach(m => {
      const td = footRow.insertCell();
      td.className = "cell-numeric canvas-pivot-grandtotal-val";
      td.textContent = formatValue(grandTotals[m.key] ?? 0);
    });
  }

  render();
  return container;
}

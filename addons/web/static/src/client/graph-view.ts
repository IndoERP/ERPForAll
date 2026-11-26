/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The graph view's DOM: toolbar, the chart drawn as plain SVG (no chart library), the legend, and the same figures as a
// table. The model (`graph.ts`) owns every read; this file only draws its state and asks it for changes.
import { icon } from "./icons.js";
import { formatNumeric, type NumericLocale } from "./numeric-values.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import type { GraphData, GraphMode, GraphModel } from "./graph.js";

const SVG = "http://www.w3.org/2000/svg";
const el = <K extends keyof HTMLElementTagNameMap>(tag: K, text?: string, className?: string): HTMLElementTagNameMap[K] => {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
};
/** One SVG element with its attributes (the dashboard card chart draws with it too). */
export const svg = <K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>, className?: string): SVGElementTagNameMap[K] => {
  const node = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, String(value));
  if (className) node.setAttribute("class", className);
  return node;
};
const button = (text: string, className: string, onClick: () => void): HTMLButtonElement => {
  const node = el("button", text, className);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
};
/** Series colour classes (client.css): the accent alone for one series, the eleven palette tokens in turn for several. A class, not an
 *  inline style: a strict content security policy (the browser fixture's `style-src 'self'`) drops style attributes. */
const colour = (index: number, count: number): string => count === 1 ? "graph-s0" : `graph-s${(index % 11) + 1}`;
const MODE_LABELS: Record<GraphMode, string> = { bar: "Bar chart", line: "Line chart", pie: "Pie chart" };
const MODE_ICONS = { bar: "chart-column", line: "chart-line", pie: "chart-pie" } as const;
/** Axis labels longer than this are cut with an ellipsis; the full label is in the mark's tooltip and the table. */
const LABEL_CHARS = 16;
const cut = (label: string): string => label.length > LABEL_CHARS ? `${label.slice(0, LABEL_CHARS - 1)}…` : label;

/** What a click on a bar or slice asks the application to open: its records. */
export interface GraphTarget { domain: unknown[]; labels: string[] }
export interface GraphView { element: HTMLElement; render(): void; dispose(): void }
export interface GraphViewOptions {
  model: GraphModel;
  download?: (file: Blob) => void;
  locale: NumericLocale;
  run: (job: () => Promise<void>) => void;
  /** Open the records of a bar or slice. Absent, or `disable_linking`: nothing opens. Lines open nothing (reference `graph_renderer.js`). */
  open?: (target: GraphTarget) => Promise<void>;
  /** Called after every change the user made, once the chart is drawn (the application keeps the address current). */
  changed?: () => void;
}

export function mountGraph(options: GraphViewOptions): GraphView {
  const { model, locale, run } = options;
  let disposed = false;
  const hidden = new Set<string>(); // legend entries turned off, by label
  let asTable = false;
  const root = el("div", undefined, "graph-view");
  const toolbar = el("div", undefined, "pivot-toolbar graph-toolbar");
  toolbar.setAttribute("role", "toolbar"); toolbar.setAttribute("aria-label", "Graph actions");
  const act = (job: () => Promise<void> | void): void => run(async () => {
    try { await job(); } finally { render(); }
    if (!disposed) options.changed?.();
  });

  // ---- Measures: one at a time ----
  const wrap = el("div", undefined, "nav-dropdown");
  const trigger = el("button", undefined, "nav-dropdown-toggle pivot-measures-toggle");
  trigger.type = "button";
  trigger.append(el("span", "Measures"), icon("chevron-down"));
  const menu = el("div", undefined, "nav-dropdown-menu pivot-measures-menu");
  menu.setAttribute("role", "menu");
  wrap.append(trigger, menu);
  const dropdown = attachMenuDropdown({ trigger, menu, mode: "class-open", items: () => [...menu.querySelectorAll<HTMLElement>("[role=menuitemradio]")] });
  const drawMenu = (): void => {
    menu.replaceChildren(...model.config.measures.map(measure => {
      const on = model.current.measure === measure.id;
      const item = el("button", undefined, "dropdown-item pivot-measure-item");
      item.type = "button";
      const check = el("span", undefined, "pivot-check");
      if (on) check.append(icon("square-check-big"));
      item.append(check, el("span", measure.label));
      item.setAttribute("role", "menuitemradio"); item.setAttribute("aria-checked", String(on));
      item.dataset["measure"] = measure.id;
      item.addEventListener("click", () => { if (!on) act(() => model.setMeasure(measure.id)); });
      return item;
    }));
    dropdown.refresh();
  };

  // ---- switches, order and chart type ----
  const toggle = (label: string, onClick: () => void): HTMLButtonElement => {
    const node = button(label, "pivot-tool graph-toggle", onClick);
    node.setAttribute("aria-pressed", "false");
    return node;
  };
  const switcher = (label: string, onClick: () => void): HTMLButtonElement => {
    const node = button(label, "pivot-tool graph-toggle graph-switch", onClick);
    node.setAttribute("role", "switch"); node.setAttribute("aria-checked", "false");
    return node;
  };
  const stacked = switcher("Stacked", () => act(() => model.setLocal({ stacked: !model.current.stacked })));
  const cumulated = switcher("Cumulative", () => act(() => model.setLocal({ cumulated: !model.current.cumulated })));
  const order = el("div", undefined, "graph-group");
  order.setAttribute("role", "group"); order.setAttribute("aria-label", "Sort");
  const orderButton = (value: "desc" | "asc", label: string): HTMLButtonElement => toggle(label, () => act(() => model.setLocal({ order: model.current.order === value ? null : value })));
  const descending = orderButton("desc", "Descending"), ascending = orderButton("asc", "Ascending");
  order.append(descending, ascending);
  const modes = el("div", undefined, "graph-group");
  modes.setAttribute("role", "group"); modes.setAttribute("aria-label", "Chart type");
  const modeButtons = (["bar", "line", "pie"] as const).map(mode => {
    const node = toggle("", () => act(() => model.setLocal({ mode })));
    node.classList.add("graph-mode");
    node.dataset["mode"] = mode;
    node.setAttribute("aria-label", MODE_LABELS[mode]); node.title = MODE_LABELS[mode];
    node.append(icon(MODE_ICONS[mode]));
    return node;
  });
  modes.append(...modeButtons);
  const tableToggle = toggle("Show data", () => { asTable = !asTable; render(); });
  const end = el("div", undefined, "pivot-toolbar-end");
  end.append(stacked, cumulated, order, modes, tableToggle);
  const download = button("Export chart", "pivot-tool graph-download", () => {
    const chart = figure.querySelector("svg");
    if (!chart || !options.download) return;
    // Resolve the live theme into presentation attributes for a standalone SVG.
    const copy = chart.cloneNode(true) as SVGSVGElement;
    const sources = [...chart.querySelectorAll("*")], targets = [...copy.querySelectorAll("*")];
    for (const [index, source] of sources.entries()) {
      const style = getComputedStyle(source);
      for (const property of ["fill", "stroke", "stroke-width", "font-family", "font-size"]) {
        const value = style.getPropertyValue(property);
        if (value) targets[index]!.setAttribute(property, value);
      }
    }
    options.download(new Blob([new XMLSerializer().serializeToString(copy)], { type: "image/svg+xml" }));
  });
  if (options.download) end.append(download);
  toolbar.append(wrap, end);

  const figure = el("div", undefined, "graph-figure");
  const legend = el("div", undefined, "graph-legend");
  legend.setAttribute("role", "group"); legend.setAttribute("aria-label", "Legend");
  const status = el("p", undefined, "pivot-status muted");
  status.setAttribute("role", "status");
  root.append(toolbar, figure, legend, status);

  const value = (data: GraphData, n: number): string => formatNumeric(n, data.measure.kind, locale, data.measure.kind === "float" ? 2 : undefined);
  const linking = (): boolean => options.open !== undefined && !model.config.disableLinking;
  const open = (domain: unknown[] | undefined, labels: string[]): void => {
    if (!domain || !linking()) return;
    run(async () => { await options.open!({ domain, labels }); });
  };
  const mark = <T extends SVGElement>(node: T, tip: string, domain: unknown[] | undefined, labels: string[], clickable: boolean): T => {
    const title = document.createElementNS(SVG, "title");
    title.textContent = tip;
    node.append(title);
    if (clickable && domain && linking()) {
      node.classList.add("graph-link");
      node.addEventListener("click", () => open(domain, labels));
    }
    return node;
  };

  const drawAxes = (chart: SVGSVGElement, data: GraphData, box: { x: number; y: number; w: number; h: number }, min: number, max: number): ((v: number) => number) => {
    const scale = (v: number): number => box.y + box.h - ((v - min) / (max - min || 1)) * box.h;
    // An integer measure (Count included) has no fractional values, so its ticks are whole numbers: formatNumeric refuses 0.25 as an integer.
    const step = data.measure.kind === "integer" ? Math.max(1, niceStep((max - min) / 4)) : niceStep((max - min) / 4);
    for (let tick = Math.ceil(min / step) * step; tick <= max + step / 1e6; tick += step) {
      const y = scale(tick);
      chart.append(svg("line", { x1: box.x, x2: box.x + box.w, y1: y, y2: y }, tick === 0 ? "graph-axis" : "graph-grid"));
      const text = svg("text", { x: box.x - 8, y: y + 4, "text-anchor": "end" }, "graph-tick");
      text.textContent = value(data, tick);
      chart.append(text);
    }
    const band = box.w / Math.max(data.labels.length, 1);
    data.labels.forEach((label, index) => {
      const text = svg("text", { x: box.x + band * (index + 0.5), y: box.y + box.h + 18, "text-anchor": "middle" }, "graph-tick");
      text.textContent = cut(label);
      chart.append(text);
    });
    return scale;
  };

  const drawChart = (data: GraphData, mode: GraphMode, isStacked: boolean): SVGSVGElement => {
    const width = Math.max(figure.clientWidth || 800, 320), height = 360;
    const chart = svg("svg", { viewBox: `0 0 ${width} ${height}`, width: "100%", height, role: "img" }, "graph-chart");
    chart.setAttribute("aria-label", `${MODE_LABELS[mode]} of ${data.measure.label}. The figures are in the data table (Show data).`);
    if (mode === "pie") {
      const slices = data.slices.filter(slice => !hidden.has(slice.label));
      const total = slices.reduce((sum, slice) => sum + slice.value, 0);
      const r = Math.min(width, height) / 2 - 16, cx = width / 2, cy = height / 2;
      let angle = -Math.PI / 2;
      for (const slice of slices) {
        const sweep = (slice.value / total) * Math.PI * 2;
        const share = `${Math.round((slice.value / total) * 1000) / 10}%`;
        const tip = `${slice.label}: ${value(data, slice.value)} (${share})`;
        const fill = colour(slice.index, data.slices.length);
        // a single slice is a whole circle, which an arc path cannot draw
        const shape = slices.length === 1 ? svg("circle", { cx, cy, r }, `graph-slice ${fill}`) : svg("path", { d: arc(cx, cy, r, angle, angle + sweep) }, `graph-slice ${fill}`);
        chart.append(mark(shape, tip, slice.domain, [slice.label], true));
        angle += sweep;
      }
      return chart;
    }
    const shown = data.series.map((series, index) => ({ series, index })).filter(item => !hidden.has(item.series.label));
    const stack = isStacked && shown.length > 1;
    const tops = data.labels.map((_, x) => shown.reduce((sum, item) => sum + item.series.points[x]!.value, 0));
    const values = stack ? tops : shown.flatMap(item => item.series.points.map(p => p.value));
    const max = Math.max(0, ...values), min = Math.min(0, ...shown.flatMap(item => item.series.points.map(p => p.value)));
    const box = { x: 72, y: 12, w: width - 88, h: height - 48 };
    const scale = drawAxes(chart, data, box, min, max === min ? min + 1 : max);
    const band = box.w / Math.max(data.labels.length, 1);
    const base = new Array<number>(data.labels.length).fill(0);
    shown.forEach((item, position) => {
      const fill = colour(item.index, data.series.length);
      const points: string[] = [];
      item.series.points.forEach((point, x) => {
        const from = stack ? base[x]! : 0, to = from + point.value;
        if (stack) base[x] = to;
        const labels = [data.labels[x]!, ...(data.series.length > 1 ? [item.series.label] : [])];
        const tip = `${labels.join(" / ")}: ${value(data, point.value)}`;
        if (mode === "bar") {
          const w = stack ? band * 0.6 : (band * 0.7) / shown.length;
          const left = box.x + band * x + band * (stack ? 0.2 : 0.15) + (stack ? 0 : w * position);
          const top = scale(Math.max(from, to)), bottom = scale(Math.min(from, to));
          chart.append(mark(svg("rect", { x: left, y: top, width: Math.max(w - 2, 1), height: Math.max(bottom - top, 0) }, `graph-bar ${fill}`), tip, point.domain, labels, true));
        } else {
          const cx = box.x + band * (x + 0.5), cy = scale(to);
          points.push(`${cx},${cy}`);
          chart.append(mark(svg("circle", { cx, cy, r: 4 }, `graph-point ${fill}`), tip, point.domain, labels, false));
        }
      });
      if (mode === "line") chart.insertBefore(svg("polyline", { points: points.join(" ") }, `graph-line ${fill}`), chart.querySelector(".graph-point"));
    });
    return chart;
  };

  const drawTable = (data: GraphData, mode: GraphMode): HTMLTableElement => {
    const table = el("table", undefined, "pivot-table graph-table");
    const caption = el("caption", data.measure.label, "visually-hidden");
    const head = el("thead"), row = el("tr");
    const first = el("th", "Group"); first.scope = "col"; row.append(first);
    const columns = mode === "pie" ? [data.measure.label] : data.series.map(series => series.label);
    for (const label of columns) { const th = el("th", label, "pivot-measure-header"); th.scope = "col"; row.append(th); }
    head.append(row);
    const body = el("tbody");
    const lines = mode === "pie" ? data.slices.map(slice => ({ label: slice.label, cells: [{ value: slice.value, domain: slice.domain }] }))
      : data.labels.map((label, x) => ({ label, cells: data.series.map(series => series.points[x]!) }));
    for (const line of lines) {
      const tr = el("tr");
      const th = el("th", line.label, "pivot-row-header"); th.scope = "row"; tr.append(th);
      line.cells.forEach((cell, index) => {
        const td = el("td", undefined, "pivot-cell");
        const text = value(data, cell.value);
        if (cell.domain && linking() && mode !== "line") {
          const labels = [line.label, ...(columns.length > 1 ? [columns[index]!] : [])];
          const link = button(text, "pivot-cell-link", () => open(cell.domain, labels));
          link.setAttribute("aria-label", `${text} ${data.measure.label}, ${labels.join(", ")}: open the records`);
          td.append(link);
        } else td.textContent = text;
        tr.append(td);
      });
      body.append(tr);
    }
    table.append(caption, head, body);
    return table;
  };

  const drawLegend = (data: GraphData, mode: GraphMode): void => {
    const entries = mode === "pie" ? data.slices.map(slice => ({ label: slice.label, index: slice.index, count: data.slices.length }))
      : data.series.length > 1 ? data.series.map((series, index) => ({ label: series.label, index, count: data.series.length })) : [];
    legend.replaceChildren(...entries.map(entry => {
      const on = !hidden.has(entry.label);
      const item = button("", "graph-legend-item", () => { if (on) hidden.add(entry.label); else hidden.delete(entry.label); render(); });
      const swatch = el("span", undefined, `graph-swatch ${colour(entry.index, entry.count)}`);
      item.append(swatch, el("span", entry.label));
      item.setAttribute("aria-pressed", String(on));
      item.title = on ? `Hide ${entry.label}` : `Show ${entry.label}`;
      return item;
    }));
    legend.hidden = entries.length === 0 || asTable;
  };

  const render = (): void => {
    if (disposed) return;
    const state = model.current, data = model.data();
    drawMenu();
    const local = state.mode !== "pie";
    stacked.hidden = !local; cumulated.hidden = state.mode !== "line"; order.hidden = !local;
    stacked.setAttribute("aria-checked", String(state.stacked));
    cumulated.setAttribute("aria-checked", String(state.cumulated));
    descending.setAttribute("aria-pressed", String(state.order === "desc"));
    ascending.setAttribute("aria-pressed", String(state.order === "asc"));
    for (const node of modeButtons) node.setAttribute("aria-pressed", String(node.dataset["mode"] === state.mode));
    tableToggle.setAttribute("aria-pressed", String(asTable));
    tableToggle.textContent = asTable ? "Show chart" : "Show data";
    status.textContent = "";
    download.disabled = asTable || data.unavailable || data.empty || (state.mode === "pie" && !data.slices.length);
    figure.replaceChildren(); legend.replaceChildren(); legend.hidden = true;
    if (data.unavailable) { status.textContent = "Unable to load the chart. Adjust the search or reload the page."; return; }
    if (data.empty) { status.textContent = "There is nothing to show: no record matches the search."; return; }
    if (state.mode === "pie" && !data.slices.length) { status.textContent = "A pie chart shows only values above zero, and this measure has none. Choose a bar or line chart."; return; }
    figure.append(asTable ? drawTable(data, state.mode) : drawChart(data, state.mode, state.stacked));
    drawLegend(data, state.mode);
  };

  // The chart is drawn for the width it has: a resize draws it again.
  const resize = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(() => { if (!asTable) render(); });
  resize?.observe(figure);
  render();
  return { element: root, render, dispose: () => { disposed = true; resize?.disconnect(); dropdown.dispose(); } };
}

/** A round step for about `rough` per grid line: 1, 2 or 5 times a power of ten. */
export function niceStep(rough: number): number {
  if (!(rough > 0) || !Number.isFinite(rough)) return 1;
  const power = 10 ** Math.floor(Math.log10(rough)), unit = rough / power;
  return (unit <= 1 ? 1 : unit <= 2 ? 2 : unit <= 5 ? 5 : 10) * power;
}
function arc(cx: number, cy: number, r: number, from: number, to: number): string {
  const [x1, y1, x2, y2] = [cx + r * Math.cos(from), cy + r * Math.sin(from), cx + r * Math.cos(to), cy + r * Math.sin(to)];
  return `M ${cx} ${cy} L ${x1} ${y1} A ${r} ${r} 0 ${to - from > Math.PI ? 1 : 0} 1 ${x2} ${y2} Z`;
}

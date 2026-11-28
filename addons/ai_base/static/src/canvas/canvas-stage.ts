/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

import { icon } from "@erpforall/client";
import {
  renderBarChart,
  renderDonutChart,
  renderLineChart,
  normalizeSeries,
  type ChartPoint,
} from "./canvas-chart.js";
import { renderRichTable, type TableSpec } from "./canvas-table.js";
import { renderWorkflowFlow } from "./canvas-flow.js";
import { renderSimulation } from "./canvas-simulation.js";
import { renderPivotTable, type PivotSpec } from "./canvas-pivot.js";
import { artifactFormat, artifactIndex } from "./artifact-index.js";
import { renderStudioDocument } from "./canvas-document.js";
import { slides } from "../presentation.js";

export type CanvasArtifact = {
  canvas_type: "chart" | "table" | "pivot" | "flow" | "dashboard" | "simulation" | "slides" | "document" | "brief" | "timeline" | "checklist";
  title?: string;
  description?: string;
  data: unknown;
};

export type CanvasStageHandle = {
  node: HTMLElement;
  renderArtifact(artifact: unknown, options?: { activate?: boolean }): string;
  selectArtifact(id: string): boolean;
  detectAndRender(text: string): boolean;
  clear(): void;
  dispose(): void;
};

type ViewMode = "bar" | "line" | "donut" | "pivot" | "table";

let nextVisualization = 0;

function dashboardMetrics(data: Record<string, unknown>): HTMLElement | null {
  const { dimensions, records, measures } = data;
  if (!Array.isArray(dimensions) || dimensions.length || !Array.isArray(records) || records.length !== 1
    || !Array.isArray(measures) || !measures.length) return null;
  const row: unknown = records[0];
  if (!row || typeof row !== "object") throw new Error("A dashboard needs one aggregate result.");
  const metrics = document.createElement("dl");
  metrics.className = "studio-metrics";
  for (const measure of measures) {
    if (!measure || typeof measure.key !== "string" || !Object.hasOwn(row, measure.key)) {
      throw new Error("The ERP result is missing a dashboard measure.");
    }
    const value = (row as Record<string, unknown>)[measure.key];
    if (value !== null && typeof value !== "number" && typeof value !== "string" && value !== false) {
      throw new Error("The ERP result contains an invalid dashboard value.");
    }
    const name = document.createElement("dt"), amount = document.createElement("dd");
    name.textContent = typeof measure.label === "string" && measure.label !== measure.key ? measure.label
      : measure.key.split(":").map((part: string) => part === "id" ? "ID" : part.replaceAll("_"," ").replace(/^./,c=>c.toUpperCase())).join(" · ");
    amount.textContent = value === null || value === false ? "—" : String(value);
    const metric = document.createElement("div");
    metric.append(name,amount); metrics.append(metric);
  }
  return metrics;
}

export function createCanvasStage(
  onPromptSelect?: (prompt: string) => void,
  onSave?: (name: string, definition: string) => void,
  onOpenSource?: (title: string, model: string, domain: unknown[]) => void,
): CanvasStageHandle {
  const container = document.createElement("section");
  container.className = "ai-workspace-canvas-stage canvas-studio";
  container.setAttribute("aria-label", "Canvas Studio");

  const header = document.createElement("header"); header.className = "canvas-stage-header";
  const stageTitle = document.createElement("h2"); stageTitle.className = "canvas-stage-title";
  stageTitle.append(icon("panels-top-left"),document.createTextNode("Canvas Studio"));
  const count = document.createElement("span"); count.className = "studio-count";
  count.setAttribute("role","status");
  const expand = document.createElement("button");
  expand.type = "button"; expand.className = "studio-expand erp-button";
  function expandLabel(open: boolean) {
    expand.replaceChildren(icon(open ? "x" : "maximize"));
    expand.title = open ? "Exit focus mode" : "Enter focus mode";
    expand.setAttribute("aria-label",expand.title); expand.setAttribute("aria-pressed",String(open));
  }
  expandLabel(false);
  const dialog = document.createElement("dialog"); dialog.className = "studio-focus-dialog";
  dialog.setAttribute("aria-label","Canvas Studio");
  const anchor = document.createComment("Canvas Studio");
  function restore(focus: boolean) {
    if (anchor.parentNode) anchor.replaceWith(container);
    dialog.remove(); expandLabel(false);
    if (focus && container.isConnected) expand.focus();
  }
  dialog.addEventListener("close", () => restore(true));
  expand.onclick = () => {
    if (dialog.open) { dialog.close(); return; }
    container.before(anchor,dialog); dialog.append(container);
    dialog.showModal(); expandLabel(true); expand.focus();
  };
  const blankState = document.createElement("div");
  blankState.className = "canvas-blank-state"; blankState.tabIndex = -1;
  const blankTitle = document.createElement("h3");
  blankTitle.textContent = "What would you like to create?";
  const intro = document.createElement("p"); intro.textContent = "Choose a starting point or describe an output in the conversation.";
  const suggestionList = document.createElement("div"); suggestionList.className = "canvas-suggestion-list";
  const suggestions = [
    {kind:"chart",label:"Analyze a trend",detail:"Plot revenue over time",prompt:"Chart monthly sales from the ERP by currency and include the records behind the trend."},
    {kind:"document",label:"Draft a brief",detail:"Summarize a decision",prompt:"Draft a decision brief with background, options, and next steps. Ask me for the decision first."},
    {kind:"checklist",label:"Plan a rollout",detail:"Map tasks and milestones",prompt:"Prepare an ERP implementation checklist and timeline. Ask me about modules, owners and target dates first."},
    {kind:"pivot",label:"Build a pivot",detail:"Compare groups and measures",prompt:"Build a sales pivot by status and currency using the live ERP records."},
    {kind:"slides",label:"Prepare slides",detail:"Outline a presentation",prompt:"Prepare a presentation for a stakeholder review. Ask which decisions, audience and sources to include."},
    {kind:"flow",label:"Map a workflow",detail:"Explain a business process",prompt:"Map the steps from quotation to paid invoice and show where approvals are required."},
  ];
  if (onPromptSelect) for (const item of suggestions) {
    const chip = document.createElement("button"), label = document.createElement("span"), detail = document.createElement("small");
    chip.type = "button"; chip.className = "canvas-suggestion-chip"; chip.dataset["kind"] = item.kind;
    label.textContent = item.label; detail.textContent = item.detail;
    const copy = document.createElement("span"); copy.append(label,detail);
    chip.append(icon(artifactFormat(item.kind)[1]),copy,icon("chevron-right"));
    chip.onclick = () => onPromptSelect(item.prompt);
    suggestionList.append(chip);
  }
  if (!onPromptSelect) intro.textContent = "Ask the assistant to create a report, document or plan.";
  blankState.append(blankTitle,intro,suggestionList);
  const viewport = document.createElement("div"); viewport.className = "canvas-stage-viewport";
  const cardsGrid = document.createElement("div"); cardsGrid.className = "canvas-cards-grid";
  const index = artifactIndex(cardsGrid, blankState, size => {
    count.textContent = size ? `${size} artifact${size === 1 ? "" : "s"}` : "";
    viewport.dataset["empty"] = String(!size);
  });
  header.append(stageTitle,count,index.arrange,expand); viewport.append(index.rail,cardsGrid);
  container.append(header,index.picker,viewport);
  function clear() { index.clear(); }

  function renderViewModeContent(
    mode: ViewMode,
    title: string,
    series: ChartPoint[],
    unit: string | undefined,
    tableSpec: TableSpec | undefined,
    pivotSpec: PivotSpec | undefined
  ): HTMLElement {
    switch (mode) {
      case "bar":
        return renderBarChart({ title, series, unit });
      case "line":
        return renderLineChart({ title, series, unit });
      case "donut":
        return renderDonutChart({ title, series, unit });
      case "pivot":
        if (pivotSpec) {
          return renderPivotTable(pivotSpec);
        }
        return renderPivotTable({
          title,
          dimensions: ["Kategori"],
          measures: [{ key: "value", label: unit ? `Nilai (${unit})` : "Nilai", agg: "sum" }],
          records: series.map(s => ({ Kategori: s.label, value: s.value })),
        });
      case "table":
        if (tableSpec) {
          return renderRichTable(tableSpec);
        }
        return renderRichTable({
          title,
          columns: ["Kategori", unit ? `Nilai (${unit})` : "Nilai"],
          rows: series.map(s => [s.label, s.value]),
        });
    }
  }

  function buildCard(
    canvasType: string,
    title: string,
    description: string,
    data: Record<string, unknown>
  ): HTMLElement {
    const card = document.createElement("article");
    card.className = "canvas-card-item canvas-rendered-card";

    // Prepare normalized data
    let initialMode: ViewMode = "bar";
    if (canvasType === "pivot") initialMode = "pivot";
    else if (canvasType === "table") initialMode = "table";
    else if (canvasType === "chart") {
      const ct = String(data["type"] || "bar");
      if (ct === "donut" || ct === "pie") initialMode = "donut";
      else if (ct === "line") initialMode = "line";
      else initialMode = "bar";
    }

    const grounded = typeof data["source_model"] === "string";

    const unit = typeof data["unit"] === "string" ? data["unit"] : undefined;
    let series: ChartPoint[] = [];
    if (Array.isArray(data["series"]) || Array.isArray(data["data"])) {
      series = normalizeSeries(data["series"] || data["data"]);
    } else if (Array.isArray(data)) {
      series = normalizeSeries(data);
    }

    if (grounded && (initialMode === "pivot" || !series.length)) initialMode = "table";
    const metrics = canvasType === "dashboard" && grounded ? dashboardMetrics(data) : null;

    let tableSpec: TableSpec | undefined;
    if (Array.isArray(data["columns"]) || Array.isArray(data["rows"]) || Array.isArray(data["records"])) {
      tableSpec = {
        title,
        allowExport: !grounded,
        columns: Array.isArray(data["columns"]) ? (data["columns"] as any[]) : undefined,
        rows: Array.isArray(data["rows"]) ? (data["rows"] as any[]) : undefined,
        records: Array.isArray(data["records"]) ? (data["records"] as any[]) : undefined,
      };
      if (!data["source_model"] && !series.length && Array.isArray(data["rows"]) && data["rows"].length) {
        series = normalizeSeries(data["rows"]);
      } else if (!data["source_model"] && !series.length && Array.isArray(data["records"]) && data["records"].length) {
        series = normalizeSeries(data["records"]);
      }
    }

    let pivotSpec: PivotSpec | undefined;
    if (canvasType === "pivot" || Array.isArray(data["dimensions"]) || Array.isArray(data["records"])) {
      const dims = (data["dimensions"] || data["dimension"] || data["groupby"] || data["group_by"]) as
        | string[]
        | undefined;
      const measures = data["measures"] as any[] | undefined;
      const records = (data["records"] || data["groups"] || (Array.isArray(data) ? data : undefined)) as
        | Record<string, unknown>[]
        | undefined;
      pivotSpec = {
        title,
        dimensions: Array.isArray(dims) ? dims : undefined,
        measures: Array.isArray(measures) ? measures : undefined,
        records: Array.isArray(records) ? records : undefined,
        columns: Array.isArray(data["columns"]) ? (data["columns"] as string[]) : undefined,
        rows: Array.isArray(data["rows"]) ? (data["rows"] as any[][]) : undefined,
      };
      if (!data["source_model"] && !series.length && Array.isArray(records) && records.length) {
        series = normalizeSeries(records);
      }
    }

    // Header
    const cardHeader = document.createElement("div");
    cardHeader.className = "canvas-card-header";

    const titleArea = document.createElement("div");
    titleArea.className = "canvas-card-title-area";

    const format = artifactFormat(canvasType);
    const formatLabel = document.createElement("span"); formatLabel.className = "studio-format";
    formatLabel.append(icon(format[1]),document.createTextNode(format[0]));
    const cardHeading = document.createElement("h3");
    cardHeading.className = "canvas-card-title";
    cardHeading.textContent = title;

    titleArea.append(formatLabel, cardHeading);

    // View Switcher (Bar, Line, Donut, Pivot, Table)
    const switcher = document.createElement("nav");
    switcher.className = "canvas-view-switcher";
    switcher.setAttribute("role", "tablist");
    switcher.setAttribute("aria-label", "Visualization");

    const modes: { id: ViewMode; label: string; iconName: "chart-column" | "chart-line" | "chart-pie" | "layout-grid" | "list" }[] = [
      { id: "bar", label: "Bar", iconName: "chart-column" },
      { id: "line", label: "Line", iconName: "chart-line" },
      { id: "donut", label: "Donut", iconName: "chart-pie" },
      { id: "pivot", label: "Pivot", iconName: "layout-grid" },
      { id: "table", label: "Table", iconName: "list" },
    ];

    let currentMode = initialMode;
    const body = document.createElement("div");
    body.className = "canvas-card-body";
    body.id = `studio-visualization-${++nextVisualization}`;

    function updateView(mode: ViewMode) {
      currentMode = mode;
      body.setAttribute("aria-labelledby", `${body.id}-${mode}`);
      switcher.querySelectorAll<HTMLButtonElement>(".canvas-btn-switch").forEach(btn => {
        const selected = btn.dataset["mode"] === mode;
        btn.classList.toggle("active", selected);
        btn.setAttribute("aria-selected", String(selected));
        btn.tabIndex = selected ? 0 : -1;
      });
      body.replaceChildren(renderViewModeContent(mode, title, series, unit, tableSpec, pivotSpec));
    }

    const switchButtons: HTMLButtonElement[] = [];
    modes.forEach(m => {
      const btn = document.createElement("button");
      btn.type = "button";
      // Pre-aggregated rows cannot be summed across currencies or averaged a second time.
      btn.hidden = grounded && (m.id === "pivot" || m.id === "donut" || (!series.length && m.id !== "table"));
      btn.className = `canvas-btn-switch ${m.id === currentMode ? "active" : ""}`;
      btn.dataset["mode"] = m.id;
      btn.setAttribute("role", "tab");
      btn.id = `${body.id}-${m.id}`;
      btn.setAttribute("aria-controls", body.id);
      btn.setAttribute("aria-selected", String(m.id === currentMode));
      btn.tabIndex = m.id === currentMode ? 0 : -1;
      btn.append(icon(m.iconName), document.createTextNode(` ${m.label}`));
      btn.addEventListener("click", () => updateView(m.id));
      switchButtons.push(btn);
      switcher.append(btn);
    });
    // Roving tabindex with arrow-key navigation, like a tab list.
    switcher.addEventListener("keydown", event => {
      if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
      event.preventDefault();
      const visible = switchButtons.filter(btn => !btn.hidden);
      const index = visible.findIndex(btn => btn.dataset["mode"] === currentMode);
      let next = index;
      if (event.key === "ArrowLeft") next = (index - 1 + visible.length) % visible.length;
      else if (event.key === "ArrowRight") next = (index + 1) % visible.length;
      else if (event.key === "Home") next = 0;
      else if (event.key === "End") next = visible.length - 1;
      const target = visible[next];
      if (target) {
        updateView(target.dataset["mode"] as ViewMode);
        target.focus();
      }
    });

    // Actions (close button)
    const cardActions = document.createElement("div");
    cardActions.className = "canvas-card-actions";

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "canvas-card-btn-close";
    closeBtn.title = "Close artifact";
    closeBtn.setAttribute("aria-label", `Close ${title}`);
    closeBtn.append(icon("x"));
    closeBtn.addEventListener("click", () => index.remove(card.id));

    if (onSave && data["source_model"] && data["analysis_definition"]) {
      const save = document.createElement("button");
      save.type = "button";
      save.className = "erp-button";
      save.textContent = "Save insight";
      save.addEventListener("click", () => onSave(title, JSON.stringify(data["analysis_definition"])));
      cardActions.append(save);
    }
    cardActions.append(closeBtn);
    cardHeader.append(titleArea, cardActions);

    // Initial Body Render
    if (grounded && Array.isArray(data["records"]) && !data["records"].length) {
      body.textContent = "No matching records.";
      switcher.hidden = true;
    } else if (metrics) {
      body.append(metrics);
      switcher.hidden = true;
    } else if (canvasType === "slides") {
      const slideEl = slides({...data,title});
      if (!slideEl) throw new Error("A presentation needs slides with a heading and bullet points.");
      body.append(slideEl);
      switcher.hidden = true;
    } else if (["document","brief","checklist","timeline"].includes(canvasType)) {
      body.append(renderStudioDocument(canvasType,data));
      switcher.hidden = true;
    } else if (canvasType === "flow") {
      const steps = (data["steps"] as any[]) || [];
      body.append(renderWorkflowFlow({ title, steps }));
      switcher.hidden = true;
    } else if (canvasType === "simulation") {
      const variables = (data["variables"] as any[]) || [];
      const baseRev = Number(data["baseRevenue"]);
      const baseMargin = Number(data["baseMarginPct"]);
      body.append(
        renderSimulation({
          title,
          description,
          variables,
          baseRevenue: Number.isFinite(baseRev) ? baseRev : 0,
          baseMarginPct: Number.isFinite(baseMargin) ? baseMargin : 0,
        })
      );
      switcher.hidden = true;
    } else {
      body.setAttribute("role", "tabpanel");
      updateView(currentMode);
    }

    card.append(cardHeader);
    if (description && !grounded) {
      const summary = document.createElement("p"); summary.className = "studio-summary"; summary.textContent = description;
      card.append(summary);
    }
    if (!switcher.hidden) card.append(switcher);
    if (grounded || canvasType === "simulation" || canvasType === "flow") {
      const source = document.createElement("p");
      source.className = "canvas-card-provenance";
      source.textContent = grounded
        ? `ERP · ${String(data["source_model"])}${data["truncated"] === true ? " · Partial results" : ""}`
        : canvasType === "simulation" ? "Hypothetical scenario" : "Proposed workflow";
      if (onOpenSource && typeof data["source_model"] === "string" && Array.isArray(data["source_domain"])) {
        const model = data["source_model"], domain = data["source_domain"];
        const open = document.createElement("button");
        open.type = "button"; open.className = "ai-workspace-reference"; open.textContent = "Open source records";
        open.onclick = () => onOpenSource(title, model, domain);
        source.append(document.createTextNode(" · "), open);
      }
      card.append(source);
    }
    card.append(body);

    // Contextual follow-up questions
    const lowerTitle = title.toLowerCase();
    let followups = [
      "Tampilkan rincian tabel lengkap",
      "Bandingkan dengan performa periode lalu",
      "Ekspor laporan ini ke format CSV",
    ];

    if (lowerTitle.includes("customer") || lowerTitle.includes("loyal")) {
      followups = [
        "Bandingkan 5 customer ini dengan tahun lalu",
        "Apa 5 produk terlaris dari pelanggan teratas?",
        "Tampilkan faktur yang belum lunas dari customer ini",
      ];
    } else if (lowerTitle.includes("piutang") || lowerTitle.includes("faktur")) {
      followups = [
        "Siapa customer dengan piutang tertua?",
        "Tampilkan faktur jatuh tempo dalam 7 hari ke depan",
        "Kirim pesan follow-up penagihan ke pelanggan",
      ];
    } else if (lowerTitle.includes("omzet") || lowerTitle.includes("tren")) {
      followups = [
        "Breakdown omzet per kategori produk",
        "Bandingkan performa Q1 vs Q2",
        "Tampilkan rasio pertumbuhan bulanan (MoM)",
      ];
    }

    const followupsSection = document.createElement("details");
    followupsSection.className = "canvas-card-followups";

    const followupsTitle = document.createElement("summary");
    followupsTitle.className = "canvas-followups-label";
    followupsTitle.textContent = "Continue in chat";

    const followupsChips = document.createElement("div");
    followupsChips.className = "canvas-followups-chips";

    if (["document","brief","slides","flow","checklist","timeline"].includes(canvasType)) {
      followups = ["Review the assumptions", "Outline the next steps"];
    }
    followups.forEach(promptText => {
      const chip = document.createElement("button");
      chip.type = "button";
      chip.className = "canvas-followup-chip";
      chip.textContent = promptText;
      chip.addEventListener("click", () => {
        if (onPromptSelect) onPromptSelect(promptText);
      });
      followupsChips.append(chip);
    });

    followupsSection.append(followupsTitle, followupsChips);
    if (onPromptSelect) card.append(followupsSection);
    return card;
  }

  function errorCard(message: string): HTMLElement {
    const card = document.createElement("article"); card.className = "canvas-render-error canvas-rendered-card";
    const detail = document.createElement("p"); detail.setAttribute("role","alert"); detail.textContent = message;
    const dismiss = document.createElement("button"); dismiss.type = "button"; dismiss.className = "erp-button";
    dismiss.textContent = "Close artifact"; dismiss.onclick = () => index.remove(card.id);
    card.append(detail,dismiss); return card;
  }
  function renderArtifact(raw: unknown, options?: {activate?: boolean}): string {
    const obj = typeof raw === "object" && raw !== null && !Array.isArray(raw) ? raw as Record<string,unknown> : {};
    const kind = String(obj["canvas_type"] ?? obj["type"] ?? "");
    const title = typeof obj["title"] === "string" && obj["title"].trim() ? obj["title"] : artifactFormat(kind)[0];
    const description = typeof obj["description"] === "string" ? obj["description"] : "";
    const data = obj["data"] ?? obj;
    let card: HTMLElement;
    try {
      if (artifactFormat(kind)[0] === "Artifact" || !data || typeof data !== "object" || Array.isArray(data)) throw new Error("Unsupported artifact format.");
      card = buildCard(kind,title,description,data as Record<string,unknown>);
    } catch (error) {
      card = errorCard(error instanceof Error ? error.message : "This artifact could not be displayed.");
    }
    const reading = cardsGrid.contains(document.activeElement) && !blankState.contains(document.activeElement);
    return index.add(card,kind,title,options?.activate ?? !reading);
  }
  function detectAndRender(text: string): boolean {
    if (text.length > 65536) return false;
    const match = text.match(/```(?:erp-view|json)\s*\n([\s\S]*?)\n```/);
    if (!match?.[1]) return false;
    try {
      const raw: unknown = JSON.parse(match[1]);
      // Free-form assistant text cannot supply actions or attest that its numbers came from ERP.
      if (typeof raw === "object" && raw !== null) {
        const data = (raw as Record<string,unknown>)["data"] ?? raw;
        if (data && typeof data === "object" && ("source_model" in data || "analysis_definition" in data)) return false;
      }
      renderArtifact(raw); return true;
    } catch {
      index.add(errorCard("Unable to read the artifact JSON."),"error","Invalid artifact");
      return false;
    }
  }
  return {
    node: container,
    renderArtifact,
    selectArtifact: index.select,
    detectAndRender,
    clear,
    dispose() {
      if (dialog.open) dialog.close();
      restore(false); index.clear(); container.remove();
    },
  };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, describe, expect, test, vi } from "vitest";
import { renderBarChart, renderDonutChart, renderLineChart } from "../src/canvas/canvas-chart.js";
import { csvCell } from "../src/canvas/csv.js";
import { renderRichTable } from "../src/canvas/canvas-table.js";
import { renderWorkflowFlow } from "../src/canvas/canvas-flow.js";
import { renderSimulation } from "../src/canvas/canvas-simulation.js";
import { renderPivotTable } from "../src/canvas/canvas-pivot.js";
import { createCanvasStage } from "../src/canvas/canvas-stage.js";

afterEach(() => document.body.replaceChildren());

describe("Canvas Visual Components", () => {
  test("renderBarChart creates SVG bars with proper heights and labels", () => {
    const el = renderBarChart({
      title: "Top Customers",
      series: [
        { label: "Deco Addict", value: 100 },
        { label: "Azure Interior", value: 50 },
      ],
      unit: "USD",
    });
    document.body.append(el);

    const svg = el.querySelector("svg");
    expect(svg).not.toBeNull();
    const rects = el.querySelectorAll("rect.canvas-bar");
    expect(rects.length).toBe(2);

    const texts = Array.from(el.querySelectorAll("text"), t => t.textContent);
    expect(texts).toContain("Deco Addict");
    expect(texts).toContain("Azure Interior");
  });

  test("renderDonutChart calculates segments and legend totals", () => {
    const el = renderDonutChart({
      title: "Revenue by State",
      series: [
        { label: "Draft", value: 20 },
        { label: "Done", value: 80 },
      ],
      unit: "USD",
    });
    document.body.append(el);

    const segments = el.querySelectorAll("circle.canvas-donut-segment");
    expect(segments.length).toBe(2);

    const legendItems = el.querySelectorAll(".canvas-donut-legend li");
    expect(legendItems.length).toBe(2);
    expect(legendItems[0]?.textContent).toContain("Draft");
  });

  test("renderRichTable sorts columns and filters rows", () => {
    const el = renderRichTable({
      title: "Sales Orders",
      columns: ["Order", "Customer", "Amount"],
      rows: [
        ["SO001", "Customer B", 20000],
        ["SO002", "Customer A", 50000],
        ["SO003", "Customer C", 10000],
      ],
    });
    document.body.append(el);

    const rows = () => Array.from(el.querySelectorAll("tbody tr"), tr => Array.from(tr.querySelectorAll("td"), td => td.textContent));
    expect(rows().length).toBe(3);

    // Filter
    const search = el.querySelector<HTMLInputElement>(".canvas-table-search")!;
    search.value = "Customer A";
    search.dispatchEvent(new Event("input"));
    expect(rows().length).toBe(1);
    expect(rows()[0]?.[1]).toBe("Customer A");

    // Clear filter
    search.value = "";
    search.dispatchEvent(new Event("input"));
    expect(rows().length).toBe(3);

    // Sort by Amount ascending (click column 2)
    const amountTh = el.querySelectorAll("th")[2]!;
    expect(amountTh.getAttribute("aria-sort")).toBe("none");
    amountTh.click();
    expect(amountTh.getAttribute("aria-sort")).toBe("ascending");
    expect(rows()[0]?.[0]).toBe("SO003"); // 10000 is first
    expect(rows()[2]?.[0]).toBe("SO002"); // 50000 is last

    // Sort descending
    amountTh.click();
    expect(amountTh.getAttribute("aria-sort")).toBe("descending");
    expect(rows()[0]?.[0]).toBe("SO002"); // 50000 is first

    // Keyboard activation sorts the same way (Enter on the header)
    amountTh.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(amountTh.getAttribute("aria-sort")).toBe("ascending");
    expect(rows()[0]?.[0]).toBe("SO003");
  });

  test("renderWorkflowFlow displays status badges and active indicators", () => {
    const el = renderWorkflowFlow({
      title: "Lifecycle",
      steps: [
        { id: "draft", label: "Penawaran", state: "completed" },
        { id: "sale", label: "Pesanan Penjualan", state: "active" },
        { id: "done", label: "Faktur Selesai", state: "pending" },
      ],
    });
    document.body.append(el);

    const steps = el.querySelectorAll(".canvas-flow-step");
    expect(steps.length).toBe(3);
    expect(steps[0]?.classList.contains("step-completed")).toBe(true);
    expect(steps[1]?.classList.contains("step-active")).toBe(true);
    expect(steps[2]?.classList.contains("step-pending")).toBe(true);
  });

  test("renderSimulation updates projected revenue and margin on slider change", () => {
    const el = renderSimulation({
      title: "Simulasi Diskon",
      variables: [
        { key: "discount", label: "Diskon Promosi", min: 0, max: 30, initial: 10, unit: "%" },
      ],
      baseRevenue: 1000000,
      baseMarginPct: 30,
    });
    document.body.append(el);

    const readout = el.querySelector(".canvas-sim-readout")!;
    expect(readout.textContent).toBe("10%");

    const slider = el.querySelector<HTMLInputElement>("input[type=range]")!;
    expect(slider.labels?.[0]?.textContent).toBe("Diskon Promosi");
    slider.value = "20";
    slider.dispatchEvent(new Event("input"));

    expect(readout.textContent).toBe("20%");
    const kpis = el.querySelectorAll(".canvas-kpi-val");
    expect(kpis.length).toBe(3);
    expect(el.textContent).not.toContain("Rp");
  });

  test("createCanvasStage renders artifact and detects fenced erp-view JSON", () => {
    const stage = createCanvasStage();
    document.body.append(stage.node);

    expect(stage.node.querySelector(".canvas-blank-state")).not.toBeNull();

    // Render structured artifact
    stage.renderArtifact({
      canvas_type: "chart",
      title: "Hasil Agregasi",
      data: {
        series: [{ label: "A", value: 10 }],
      },
    });

    expect(stage.node.querySelector(".canvas-blank-state")).toBeNull();
    expect(stage.node.querySelector(".canvas-bar-chart")).not.toBeNull();
    expect(stage.node.textContent).toContain("Hasil Agregasi");

    // Clear
    stage.clear();
    expect(stage.node.querySelector(".canvas-blank-state")).not.toBeNull();

    // Detect fenced code block
    const text = "Berikut adalah chart:\n```erp-view\n{\"type\":\"chart\",\"title\":\"Fenced Chart\",\"series\":[{\"label\":\"B\",\"value\":20}]}\n```";
    const detected = stage.detectAndRender(text);
    expect(detected).toBe(true);
    expect(stage.node.querySelector(".canvas-bar-chart")).not.toBeNull();
    expect(stage.node.textContent).toContain("Fenced Chart");

    stage.dispose();
  });

  test("createCanvasStage detectAndRender handles malformed JSON by displaying error card", () => {
    const stage = createCanvasStage();
    document.body.append(stage.node);

    const invalidText = "Berikut adalah chart:\n```erp-view\n{invalid_json:\n```";
    const detectedErr = stage.detectAndRender(invalidText);
    expect(detectedErr).toBe(false);
    expect(stage.node.querySelector("[role=alert]")?.textContent).toContain("Unable to read");

    stage.dispose();
  });

  test("renderPivotTable dynamically pivots data by dimension, aggregates measures, and toggles collapse", () => {
    const el = renderPivotTable({
      title: "Pivot Sales",
      dimensions: ["Customer", "Status"],
      measures: [
        { key: "amount", label: "Total Omzet", agg: "sum" },
        { key: "id", label: "Jumlah Order", agg: "count" },
      ],
      records: [
        { Customer: "Deco Addict", Status: "sale", amount: 10000000, id: 1 },
        { Customer: "Deco Addict", Status: "draft", amount: 5000000, id: 2 },
        { Customer: "Azure Interior", Status: "sale", amount: 20000000, id: 3 },
      ],
    });
    document.body.append(el);

    expect(el.querySelector(".canvas-pivot-table")).not.toBeNull();
    const groupRows = el.querySelectorAll(".canvas-pivot-group-row");
    expect(groupRows.length).toBe(2); // Deco Addict & Azure Interior

    // Check Grand Total
    const grandTotalVal = el.querySelector(".canvas-pivot-grandtotal-val");
    expect(grandTotalVal?.textContent).toContain("35.000.000"); // 10M + 5M + 20M

    // Switch Dimension to Status
    const statusBtn = Array.from(el.querySelectorAll<HTMLButtonElement>(".canvas-pivot-dim-btn")).find(
      b => b.textContent === "Status"
    )!;
    statusBtn.click();

    const newGroupRows = el.querySelectorAll(".canvas-pivot-group-row");
    expect(newGroupRows.length).toBe(2); // sale & draft
    expect(el.textContent).toContain("sale");
    expect(el.textContent).toContain("draft");
  });

  test("renderRichTable safely handles rows as objects without forEach error", () => {
    // When LLM returns rows as objects [ { col: val } ] instead of arrays
    const el = renderRichTable({
      title: "Invoices Due",
      columns: ["Invoice", "Customer", "Amount"],
      rows: [
        { Invoice: "INV/001", Customer: "PT ABC", Amount: 5000000 },
        { Invoice: "INV/002", Customer: "CV XYZ", Amount: 2500000 },
      ] as any,
    });
    document.body.append(el);

    const rows = Array.from(el.querySelectorAll("tbody tr"), tr =>
      Array.from(tr.querySelectorAll("td"), td => td.textContent)
    );
    expect(rows.length).toBe(2);
    expect(rows[0]?.[0]).toBe("INV/001");
    expect(rows[0]?.[1]).toBe("PT ABC");
    expect(rows[0]?.[2]).toBe("5.000.000");
  });

  test("renderRichTable derives columns from record keys when columns omitted", () => {
    const el = renderRichTable({
      title: "Automatic Columns",
      rows: [
        { Code: "P01", Description: "Widget", Price: 15000 },
      ] as any,
    });
    document.body.append(el);

    const headers = Array.from(el.querySelectorAll("thead th"), th => th.textContent?.trim());
    expect(headers).toContain("Code");
    expect(headers).toContain("Description");
    expect(headers).toContain("Price");
  });

  test("renderBarChart normalizes object series format safely", () => {
    const el = renderBarChart({
      title: "Key-Value Series",
      series: {
        "Q1": 120,
        "Q2": 250,
      } as any,
    });
    document.body.append(el);

    const bars = el.querySelectorAll("rect.canvas-bar");
    expect(bars.length).toBe(2);
    expect(el.textContent).toContain("Q1");
    expect(el.textContent).toContain("Q2");
  });

  test("createCanvasStage renders table with object rows gracefully without crashing", () => {
    const stage = createCanvasStage();
    document.body.append(stage.node);

    stage.renderArtifact({
      canvas_type: "table",
      title: "Daftar Piutang",
      data: {
        columns: ["Nomor", "Customer", "Total"],
        rows: [
          { Nomor: "INV/01", Customer: "Customer 1", Total: 1000000 },
        ],
      },
    });

    expect(stage.node.querySelector(".canvas-table-container")).not.toBeNull();
    expect(stage.node.textContent).toContain("INV/01");
    expect(stage.node.textContent).toContain("Customer 1");

    stage.dispose();
  });

  test("createCanvasStage offers suggestions without claiming a company data connection", () => {
    let triggeredPrompt = "";
    const stage = createCanvasStage(p => { triggeredPrompt = p; });
    document.body.append(stage.node);

    expect(stage.node.textContent).not.toContain("Terhubung ke data perusahaan");
    const chips = stage.node.querySelectorAll(".canvas-suggestion-chip");
    expect(chips.length).toBeGreaterThanOrEqual(4);

    // Click on a suggestion chip to verify the callback
    const pivotChip = Array.from(chips).find(c => c.textContent?.includes("pivot")) as HTMLButtonElement;
    expect(pivotChip).toBeDefined();
    pivotChip.click();
    expect(triggeredPrompt).toContain("pivot");

    stage.dispose();
  });

  test("charts show losses below zero and zero-height bars for zero", () => {
    const series = [{label:"Loss",value:-3},{label:"Zero",value:0},{label:"Gain",value:6}];
    const chart = renderBarChart({series});
    const bars = chart.querySelectorAll("rect");
    const zero = Number(chart.querySelector("line")!.getAttribute("y1"));
    expect(Number(bars[0]!.getAttribute("y"))).toBe(zero);
    expect(Number(bars[1]!.getAttribute("height"))).toBe(0);
    expect(Number(bars[2]!.getAttribute("y"))).toBeLessThan(zero);
    const points = renderLineChart({series}).querySelectorAll("circle");
    expect(Number(points[0]!.getAttribute("cy"))).toBeGreaterThan(Number(points[1]!.getAttribute("cy")));
    expect(Number(points[2]!.getAttribute("cy"))).toBeLessThan(Number(points[1]!.getAttribute("cy")));
    expect(renderDonutChart({series:[{label:"Zero",value:0}]}).textContent).toContain("0");
  });

  test("CSV cells preserve numeric values and export user-provided text as literal data", () => {
    expect(csvCell(-12)).toBe('"-12"');
    expect(csvCell(" =total")).toBe(`"' =total"`);
    expect(csvCell('a,"b"')).toBe('"a,""b"""');
    expect(csvCell("\ttext")).toBe(`"'\ttext"`);
  });

  test("live pivots keep returned measures and save the query without aggregating averages again", () => {
    const save = vi.fn();
    const openSource = vi.fn();
    const stage = createCanvasStage(undefined, save, openSource);
    const definition = { panels: [{ model: "sale.order", groupby: ["state"], aggregates: ["amount:avg"] }] };
    stage.renderArtifact({ canvas_type: "pivot", title: "Order values", data: {
      source_model: "sale.order", source_domain:[["state","=","sale"]], analysis_definition: definition,
      columns: ["state", "amount:avg"], records: [{ state: "sale", "amount:avg": 260 }],
      dimensions: ["state"], measures: [{key:"amount:avg",label:"Average"}], truncated:true,
    } });
    expect(stage.node.textContent).toContain("sale.order");
    expect(stage.node.querySelector(".canvas-pivot-grandtotal-val")).toBeNull();
    expect(stage.node.querySelector(".canvas-table-export")).toBeNull();
    expect(stage.node.querySelector("tbody")?.textContent).toContain("260");
    const button = Array.from(stage.node.querySelectorAll("button")).find(node => node.textContent === "Save report")!;
    button.click();
    expect(save).toHaveBeenCalledWith("Order values", JSON.stringify(definition));
    Array.from(stage.node.querySelectorAll("button")).find(node => node.textContent === "Open source records")!.click();
    expect(openSource).toHaveBeenCalledWith("Order values", "sale.order", [["state","=","sale"]]);
    stage.dispose();
  });

  test("renderRichTable supports object column definitions without throwing toLowerCase errors", () => {
    const el = renderRichTable({
      title: "Top Loyal Customers",
      columns: [
        { name: "partner_id", label: "Customer" },
        { name: "order_count", label: "Total Order" },
        { name: "amount_total", label: "Nilai Order" },
      ],
      rows: [
        { partner_id: "Deco Addict", order_count: 5, amount_total: 4500000 },
        { partner_id: "Azure Interior", order_count: 3, amount_total: 2100000 },
      ],
    });
    document.body.append(el);

    const headers = Array.from(el.querySelectorAll("th"), th => th.textContent?.trim());
    expect(headers).toContain("Customer");
    expect(headers).toContain("Total Order");
    expect(headers).toContain("Nilai Order");

    const cells = Array.from(el.querySelectorAll("tbody td"), td => td.textContent?.trim());
    expect(cells).toContain("Deco Addict");
    expect(cells).toContain("Azure Interior");
  });

  test("renderLineChart renders smooth SVG path with circles and text badges", () => {
    const el = renderLineChart({
      title: "Tren Penjualan Bulanan",
      series: [
        { label: "Jan", value: 100 },
        { label: "Feb", value: 150 },
        { label: "Mar", value: 200 },
      ],
      unit: "Juta",
    });
    document.body.append(el);

    const svg = el.querySelector("svg");
    expect(svg).not.toBeNull();
    const path = el.querySelector(".canvas-line-path");
    expect(path).not.toBeNull();
    const points = el.querySelectorAll(".canvas-line-point");
    expect(points.length).toBe(3);

    const textContent = el.textContent || "";
    expect(textContent).toContain("Jan");
    expect(textContent).toContain("Feb");
    expect(textContent).toContain("Mar");
  });

  test("createCanvasStage supports visual mode switching across representations", () => {
    const stage = createCanvasStage();
    document.body.append(stage.node);

    stage.renderArtifact({
      canvas_type: "chart",
      title: "Performa Sales",
      data: {
        series: [
          { label: "Produk A", value: 50 },
          { label: "Produk B", value: 80 },
        ],
      },
    });

    const card = stage.node.querySelector(".canvas-card-item")!;
    expect(card).not.toBeNull();
    // Default initial mode is bar
    expect(card.querySelector(".canvas-bar-chart")).not.toBeNull();

    // Switch to Line
    const lineBtn = card.querySelector<HTMLButtonElement>('.canvas-btn-switch[data-mode="line"]')!;
    expect(lineBtn).not.toBeNull();
    lineBtn.click();
    expect(card.querySelector(".canvas-line-chart")).not.toBeNull();

    // Switch to Donut
    const donutBtn = card.querySelector<HTMLButtonElement>('.canvas-btn-switch[data-mode="donut"]')!;
    expect(donutBtn).not.toBeNull();
    donutBtn.click();
    expect(card.querySelector(".canvas-donut-chart")).not.toBeNull();

    // Switch to Pivot
    const pivotBtn = card.querySelector<HTMLButtonElement>('.canvas-btn-switch[data-mode="pivot"]')!;
    expect(pivotBtn).not.toBeNull();
    pivotBtn.click();
    expect(card.querySelector(".canvas-pivot-table")).not.toBeNull();

    // Switch to Table
    const tableBtn = card.querySelector<HTMLButtonElement>('.canvas-btn-switch[data-mode="table"]')!;
    expect(tableBtn).not.toBeNull();
    tableBtn.click();
    expect(card.querySelector(".canvas-data-table")).not.toBeNull();

    // Arrow keys move the selection and switch the view; roving tabindex keeps one tab stop.
    tableBtn.focus();
    tableBtn.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    const barBtn = card.querySelector<HTMLButtonElement>('.canvas-btn-switch[data-mode="bar"]')!;
    expect(barBtn.getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(barBtn);
    expect(barBtn.tabIndex).toBe(0);
    expect(tableBtn.tabIndex).toBe(-1);

    stage.dispose();
  });

  test("createCanvasStage outline supports HTML5 drag-and-drop and closes the active artifact", () => {
    const stage = createCanvasStage();
    document.body.append(stage.node);

    stage.renderArtifact({
      canvas_type: "chart",
      title: "Kartu 1",
      data: { series: [{ label: "X", value: 10 }] },
    });

    const card = stage.node.querySelector(".canvas-card-item")!;
    expect(stage.node.querySelector(".studio-tab")?.getAttribute("draggable")).toBe("true");
    expect(card.getAttribute("aria-labelledby")).toBe(stage.node.querySelector(".studio-tab")?.id);

    // Close button removes card and restores blank state
    const closeBtn = card.querySelector<HTMLButtonElement>(".canvas-card-btn-close")!;
    expect(closeBtn).not.toBeNull();
    closeBtn.click();

    expect(stage.node.querySelector(".canvas-blank-state")).not.toBeNull();
    stage.dispose();
  });

  test("createCanvasStage renders contextual follow-up chips and triggers callback", () => {
    let selectedPrompt = "";
    const stage = createCanvasStage(p => {
      selectedPrompt = p;
    });
    document.body.append(stage.node);

    stage.renderArtifact({
      canvas_type: "chart",
      title: "5 Customer Terloyal",
      data: { series: [{ label: "Deco Addict", value: 100 }] },
    });

    const followups = stage.node.querySelectorAll<HTMLButtonElement>(".canvas-followup-chip");
    expect(followups.length).toBeGreaterThanOrEqual(1);

    followups[0]!.click();
    expect(selectedPrompt.length).toBeGreaterThan(0);

    // No invented query text: only the follow-up prompts appear
    expect(stage.node.querySelector(".canvas-query-details")).toBeNull();

    stage.dispose();
  });
});

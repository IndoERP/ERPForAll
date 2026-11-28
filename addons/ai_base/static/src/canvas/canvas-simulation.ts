/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

export type SimulationVariable = {
  key: string;
  label: string;
  min: number;
  max: number;
  step?: number;
  initial: number;
  unit?: string;
};

export type SimulationSpec = {
  title?: string;
  description?: string;
  variables: SimulationVariable[];
  baseRevenue: number;
  baseMarginPct: number;
};

let nextControl = 0;

export function renderSimulation(spec: SimulationSpec): HTMLElement {
  const container = document.createElement("div");
  container.className = "canvas-simulation-container";

  const rawVars = spec.variables;
  const variables: SimulationVariable[] = Array.isArray(rawVars)
    ? rawVars.filter((v): v is SimulationVariable => Boolean(v && typeof v === "object"))
    : [];
  const { baseRevenue, baseMarginPct } = spec;

  const values: Record<string, number> = {};
  variables.forEach(v => {
    values[v.key] = typeof v.initial === "number" ? v.initial : 0;
  });

  // KPI Dashboard Cards for projected outcomes
  const kpiRow = document.createElement("div");
  kpiRow.className = "canvas-sim-kpi-row";

  const revCard = document.createElement("div");
  revCard.className = "canvas-kpi-card";
  const revLabel = document.createElement("span");
  revLabel.textContent = "Proyeksi Pendapatan";
  const revVal = document.createElement("strong");
  revVal.className = "canvas-kpi-val";
  revCard.append(revLabel, revVal);

  const marginCard = document.createElement("div");
  marginCard.className = "canvas-kpi-card";
  const marginLabel = document.createElement("span");
  marginLabel.textContent = "Proyeksi Margin Laba";
  const marginVal = document.createElement("strong");
  marginVal.className = "canvas-kpi-val";
  marginCard.append(marginLabel, marginVal);

  const deltaCard = document.createElement("div");
  deltaCard.className = "canvas-kpi-card";
  const deltaLabel = document.createElement("span");
  deltaLabel.textContent = "Perubahan Net";
  const deltaVal = document.createElement("strong");
  deltaVal.className = "canvas-kpi-val";
  deltaCard.append(deltaLabel, deltaVal);

  kpiRow.append(revCard, marginCard, deltaCard);

  function recalculate() {
    // Basic scenario formula:
    // Volume delta % and Price discount %
    const volPct = (values["volume_growth"] ?? 0) / 100;
    const discountPct = (values["discount"] ?? 0) / 100;

    const projectedRevenue = baseRevenue * (1 + volPct) * (1 - discountPct);
    const projectedMarginRate = Math.max(0, baseMarginPct - discountPct * 100);
    const projectedProfit = (projectedRevenue * projectedMarginRate) / 100;
    const baseProfit = (baseRevenue * baseMarginPct) / 100;
    const profitDelta = projectedProfit - baseProfit;

    const fmt = new Intl.NumberFormat("id-ID", { maximumFractionDigits: 0 });
    revVal.textContent = fmt.format(projectedRevenue);
    marginVal.textContent = `${projectedMarginRate.toFixed(1)}% (${fmt.format(projectedProfit)})`;

    if (profitDelta >= 0) {
      deltaVal.textContent = `+${fmt.format(profitDelta)}`;
      deltaVal.style.color = "var(--success)";
    } else {
      deltaVal.textContent = `-${fmt.format(Math.abs(profitDelta))}`;
      deltaVal.style.color = "var(--danger)";
    }
  }

  // Sliders Section
  const controls = document.createElement("div");
  controls.className = "canvas-sim-controls";

  variables.forEach(v => {
    const row = document.createElement("div");
    row.className = "canvas-sim-row";

    const header = document.createElement("div");
    header.className = "canvas-sim-header";
    const title = document.createElement("label");
    title.textContent = v.label;
    const readout = document.createElement("span");
    readout.className = "canvas-sim-readout";
    readout.textContent = `${v.initial}${v.unit || ""}`;

    header.append(title, readout);

    const slider = document.createElement("input");
    slider.type = "range";
    slider.id = `scenario-input-${++nextControl}`;
    title.htmlFor = slider.id;
    slider.min = String(v.min);
    slider.max = String(v.max);
    slider.step = String(v.step || 1);
    slider.value = String(v.initial);
    slider.className = "canvas-sim-slider";
    slider.setAttribute("aria-valuetext", readout.textContent);

    slider.addEventListener("input", () => {
      values[v.key] = Number(slider.value);
      readout.textContent = `${slider.value}${v.unit || ""}`;
      slider.setAttribute("aria-valuetext", readout.textContent);
      recalculate();
    });

    row.append(header, slider);
    controls.append(row);
  });

  container.append(kpiRow, controls);
  recalculate();
  return container;
}

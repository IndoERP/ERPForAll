/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
type Cell = string | number | boolean | null;
type DraftHost = {
  openDraft?(xmlId: string, defaults: Readonly<Record<string, string>>): Promise<void>;
  run(job: () => Promise<void>): void;
};
const quotation = { action: "sale.action_quotations", label: "sales quotation", fields: { client_order_ref: "Customer reference", origin: "Source document", note: "Terms and conditions" } };
const rfq = { action: "purchase.purchase_rfq", label: "request for quotation", fields: { origin: "Source document" } };

// The reply format is a versioned display grammar. The model supplies data, never markup or actions.
const formats = [
  'When the user requests a table, chart, slides, draft or saved record, you may reply with one fenced erp-view JSON object.',
  'Use version 1 and a title. Ordinary answers remain plain text. Use only supplied facts. Ask for missing data instead of inventing it.',
  'Table: {"version":1,"type":"table","title":"…","columns":["…"],"rows":[["…"]]}.',
  'Bar chart: {"version":1,"type":"chart","title":"…","series":[{"label":"…","value":1}]}.',
  'Slides: {"version":1,"type":"slides","title":"…","slides":[{"heading":"…","points":["…"]}]}.',
  'Existing document: {"version":1,"type":"record","title":"…","model":"purchase.order","reference":"…"}. Use only an exact reference supplied by the user. Supported models: sale.order, purchase.order, stock.picking, account.move.',
  'Unsaved form: {"version":1,"type":"draft","title":"…","form":"quotation","fields":{"client_order_ref":"…"}}.',
  'For quotation use only client_order_ref, origin or note. For rfq use only origin. A draft requires the user to open and save the form.',
].join("\n");

export function workspacePrompt(question: string): string {
  return formats + "\n\nUser question:\n" + question;
}

function tag<K extends keyof HTMLElementTagNameMap>(kind: K, value?: string) {
  const node = document.createElement(kind);
  if (value !== undefined) node.textContent = value;
  return node;
}
function obj(x: unknown): x is Record<string, unknown> { return typeof x === "object" && x !== null && !Array.isArray(x); }
function label(x: unknown): x is string { return typeof x === "string" && x.trim().length > 0; }
function cell(x: unknown): x is Cell { return x === null || typeof x === "string" || typeof x === "boolean" || typeof x === "number" && Number.isFinite(x); }
function button(text: string, click: () => void) {
  const el = tag("button", text); el.type = "button"; el.addEventListener("click", click); return el;
}
function table(view: Record<string, unknown>): HTMLElement | undefined {
  const {columns, rows} = view;
  if (!Array.isArray(columns) || !columns.length || columns.length > 24 || !columns.every(label) ||
      !Array.isArray(rows) || rows.length > 200 || !rows.every(row => Array.isArray(row) && row.length === columns.length && row.every(cell))) return;
  const scroll = tag("div"); scroll.className = "ai-answer-table";
  const grid = tag("table"), head = grid.createTHead().insertRow(), body = grid.createTBody();
  grid.setAttribute("aria-label", String(view["title"]));
  for (const column of columns) { const th = tag("th", column); th.scope = "col"; head.append(th); }
  for (const row of rows) {
    const tr = body.insertRow();
    for (const value of row) tr.insertCell().textContent = value === null ? "—" : String(value);
  }
  scroll.append(grid); return scroll;
}
function chart(view: Record<string, unknown>): HTMLElement | undefined {
  const {series} = view;
  if (!Array.isArray(series) || !series.length || series.length > 48) return;
  const points: { label: string; value: number }[] = [];
  for (const p of series) {
    if (!obj(p) || !label(p["label"]) || typeof p["value"] !== "number" || !Number.isFinite(p["value"])) return;
    points.push({ label: p["label"], value: p["value"] });
  }
  const scale = Math.max(...points.map(p => Math.abs(p["value"]))) || 1;
  const negative = points.some(p => p["value"] < 0);
  const list = tag("ul"); list.className = "ai-answer-chart"; list.classList.toggle("signed", negative);
  for (const p of points) {
    const row = tag("li"), track = tag("span"), bar = tag("span");
    track.className = "ai-answer-track"; track.setAttribute("aria-hidden", "true");
    bar.className = "ai-answer-bar";
    const width = Math.abs(p["value"]) / scale * (negative ? 50 : 100);
    bar.style.width = width + "%"; bar.style.left = (negative ? (p["value"] < 0 ? 50 - width : 50) : 0) + "%";
    track.append(bar); row.append(tag("span", p["label"]), track, tag("span", String(p["value"]))); list.append(row);
  }
  return list;
}
export function slides(view: Record<string, unknown>): HTMLElement | undefined {
  if (!Array.isArray(view["slides"]) || !view["slides"].length || view["slides"].length > 40) return;
  const deck: {heading: string; points: string[]}[] = [];
  for (const item of view["slides"]) {
    if (!obj(item) || !label(item["heading"]) || !Array.isArray(item["points"]) || item["points"].length > 32 || !item["points"].every(label)) return;
    deck.push({ heading: item["heading"], points: item["points"] });
  }
  let page = 0;
  const region = tag("section"), content = tag("div"), controls = tag("div"), pageNumber = tag("span");
  region.className = "ai-answer-deck"; region.tabIndex = 0; region.setAttribute("aria-label", String(view["title"]));
  controls.className = "ai-answer-slide-controls"; pageNumber.setAttribute("aria-live", "polite");
  const previous = button("Previous", () => show(page - 1)), next = button("Next", () => show(page + 1));
  function show(index: number) {
    page = Math.max(0, Math.min(deck.length - 1, index));
    const slide = deck[page]!, list = tag("ul");
    for (const point of slide.points) list.append(tag("li", point));
    content.replaceChildren(tag("h4", slide.heading), list);
    pageNumber.textContent = `Slide ${page + 1} of ${deck.length}`;
    previous.disabled = page === 0; next.disabled = page === deck.length - 1;
  }
  region.addEventListener("keydown", event => {
    if (event.key === "ArrowLeft" || event.key === "ArrowRight") { event.preventDefault(); show(page + (event.key === "ArrowRight" ? 1 : -1)); }
  });
  controls.append(previous, pageNumber, next); region.append(content, controls); show(0); return region;
}
function draft(view: Record<string, unknown>, env?: DraftHost): HTMLElement | undefined {
  const target = view["form"] === "quotation" ? quotation : view["form"] === "rfq" ? rfq : undefined;
  if (!target || !obj(view["fields"])) return;
  const entries = Object.entries(view["fields"]);
  if (!entries.length) return;
  const fields: Record<string,string> = {}, labels: Record<string,string> = target.fields;
  const card = tag("div"), values = tag("dl"); card.className = "ai-answer-draft";
  for(const [key, value] of entries) {
    if (!Object.hasOwn(labels, key) || !label(value)) return;
    fields[key] = value; values.append(tag("dt", labels[key]), tag("dd", value));
  }
  card.append(values);
  if (env?.openDraft) {
    const open = env.openDraft;
    card.append(button("Open " + target.label + " draft", () => env.run(() => open(target.action, fields))));
  }
  return card;
}

/** Display a provider reply as text or as one explicitly versioned ERP view. */
export function renderReply(container: HTMLElement, text: string, env?: DraftHost,
  lookup?: (model: string, reference: string) => (() => Promise<void>) | undefined, sourcedFigures = false): void {
  const match = /^\s*`{3}erp-view\s*\n([\s\S]*?)\n`{3}\s*$/.exec(text);
  if (!match || text.length > 65536) { container.textContent = text; return; }
  let view: unknown;
  try { view = JSON.parse(match[1]!); } catch { container.textContent = text; return; }
  if (!obj(view) || view["version"] !== 1 || !label(view["title"])) { container.textContent = text; return; }
  if (sourcedFigures && (view["type"] === "table" || view["type"] === "chart")) {
    container.textContent = "Request a live report to view figures from ERP records.";
    return;
  }
  let recordLink: HTMLElement | undefined;
  if (view["type"] === "record" && label(view["model"]) && label(view["reference"])) {
    const reference = view["reference"].trim();
    const open = lookup?.(view["model"], reference);
    if (open && env) recordLink = button("Open " + reference, () => env.run(open));
    else if (!env) recordLink = tag("p", reference);
  }
  const content = view["type"] === "table" ? table(view) : view["type"] === "chart" ? chart(view) :
    view["type"] === "slides" ? slides(view) : view["type"] === "draft" ? draft(view, env) : recordLink;
  if (!content) { container.textContent = text; return; }
  const panel = tag("section"); panel.className = "ai-answer-view";
  panel.append(tag("h3", view["title"]), content); container.replaceChildren(panel);
}

/** Quantitative cards in chat must arrive through a server-validated canvas. */
export function renderChatReply(container: HTMLElement, text: string, env?: DraftHost,
  lookup?: (model: string, reference: string) => (() => Promise<void>) | undefined): void {
  renderReply(container, text, env, lookup, true);
}

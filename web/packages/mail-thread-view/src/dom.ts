/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Small DOM helpers shared by the components.

/** Attribute values accepted by {@link h}: `undefined` and `false` leave the attribute off, `true` sets it empty. */
export type Attrs = Readonly<Record<string, string | number | boolean | undefined>>;

/**
 * Create an element with attributes and children.
 *
 * @param doc - the document to create it in
 * @param tag - the tag name
 * @param attrs - attributes (`class` for the class name)
 * @param children - nodes or text
 */
export function h<K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  attrs: Attrs = {},
  ...children: readonly (Node | string)[]
): HTMLElementTagNameMap[K] {
  const node = doc.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === undefined || value === false) continue;
    node.setAttribute(name, value === true ? "" : String(value));
  }
  for (const child of children) node.append(child);
  return node;
}

let counter = 0;

/** A document-unique id with a readable prefix. */
export function uniqueId(prefix: string): string {
  counter += 1;
  return `${prefix}-${counter}`;
}

/** Format a size in bytes as a readable string: e.g. "12.5 KB", "1.4 MB", "500 B". */
export function formatBytes(bytes: number): string {
  if (bytes <= 0 || !Number.isFinite(bytes)) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) {
    const kb = bytes / 1024;
    return `${kb >= 10 ? Math.round(kb) : kb.toFixed(1)} KB`;
  }
  const mb = bytes / (1024 * 1024);
  return `${mb >= 10 ? Math.round(mb) : mb.toFixed(1)} MB`;
}

/** The visually hidden class of the stylesheet. */
export const SR_ONLY = "erp-mail-sr-only";

/** A polite status region and an assertive alert region inside a component root. */
export class Announcer {
  readonly element: HTMLElement;
  readonly #polite: HTMLElement;
  readonly #assertive: HTMLElement;

  constructor(doc: Document) {
    this.#polite = h(doc, "div", { role: "status", "aria-live": "polite", "aria-atomic": "true" });
    this.#assertive = h(doc, "div", { role: "alert", "aria-live": "assertive", "aria-atomic": "true" });
    this.element = h(doc, "div", { class: SR_ONLY }, this.#polite, this.#assertive);
  }

  /** Announce text; the latest message of each urgency replaces the previous one. */
  say(text: string, urgency: "polite" | "assertive" = "polite"): void {
    const region = urgency === "polite" ? this.#polite : this.#assertive;
    region.textContent = text;
  }
}

/** A readable label for a snake_case field name: `stage_id` becomes `Stage`. */
export function humanize(name: string): string {
  const words = name.replace(/_id$/, "").replace(/_ids$/, "").split("_").filter((w) => w !== "");
  const text = words.join(" ");
  return text === "" ? name : text.charAt(0).toUpperCase() + text.slice(1);
}

/** Lower-case and strip diacritics, for matching typed text against names. */
export function normalize(text: string): string {
  return text.normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase();
}

type IconPart = readonly [tag: string, attrs: Readonly<Record<string, string>>];

/**
 * Lucide drawings of the action glyphs (ISC, with the MIT-licensed Feather-derived ones), same 24 grid and 2px stroke as the
 * client's own icon set. The vendored set in `web/vendor/lucide` does not ship these, so the licence notice travels with this
 * package in `LICENSE-LUCIDE`.
 */
const GLYPH_ICONS: Readonly<Record<string, readonly IconPart[]>> = {
  "+": [["circle", { cx: "12", cy: "12", r: "10" }], ["path", { d: "M8 14s1.5 2 4 2 4-2 4-2" }], ["path", { d: "M9 9h.01" }], ["path", { d: "M15 9h.01" }]],
  ":)": [["circle", { cx: "12", cy: "12", r: "10" }], ["path", { d: "M8 14s1.5 2 4 2 4-2 4-2" }], ["path", { d: "M9 9h.01" }], ["path", { d: "M15 9h.01" }]],
  "<": [["path", { d: "M20 18v-2a4 4 0 0 0-4-4H4" }], ["path", { d: "m9 17-5-5 5-5" }]],
  "*": [["path", { d: "m19 21-7-4-7 4V5a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2v16z" }]],
  "^": [["path", { d: "M12 17v5" }], ["path", { d: "M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" }]],
  "=": [["rect", { width: "14", height: "14", x: "8", y: "8", rx: "2", ry: "2" }], ["path", { d: "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" }]],
  "x": [["path", { d: "M3 6h18" }], ["path", { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }], ["path", { d: "M8 6V4c0-1 1-2 2-2h4c0 1 1 2 2 2v2" }], ["path", { d: "M10 11v6" }], ["path", { d: "M14 11v6" }]],
  "paperclip": [["path", { d: "m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48" }]],
  "arrow-right": [["path", { d: "M5 12h14" }], ["path", { d: "m12 5 7 7-7 7" }]],
  "->": [["path", { d: "M5 12h14" }], ["path", { d: "m12 5 7 7-7 7" }]],
};

/** The decorative icon for an action glyph, or the glyph itself as text when no drawing is known (addon glyphs). */
export function glyphNode(doc: Document, glyph: string, size = 16): HTMLElement {
  const parts = GLYPH_ICONS[glyph];
  const wrap = h(doc, "span", { class: "erp-mail-glyph", "aria-hidden": "true" });
  if (parts === undefined) {
    wrap.textContent = glyph;
    return wrap;
  }
  const ns = "http://www.w3.org/2000/svg";
  const svg = doc.createElementNS(ns, "svg");
  for (const [k, v] of Object.entries({ viewBox: "0 0 24 24", width: String(size), height: String(size), fill: "none", stroke: "currentColor", "stroke-width": "2", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" })) svg.setAttribute(k, v);
  for (const [tag, attrs] of parts) {
    const part = doc.createElementNS(ns, tag);
    for (const [k, v] of Object.entries(attrs)) part.setAttribute(k, v);
    svg.append(part);
  }
  wrap.append(svg);
  return wrap;
}

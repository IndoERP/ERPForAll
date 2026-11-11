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

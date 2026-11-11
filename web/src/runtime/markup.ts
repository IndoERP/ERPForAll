/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Markup: HTML that code, never data, marked as safe to render as HTML (
// §7.2, §8). `insert`/`out` take the HTML path only for a value `markup()` or `htmlEscape()` created;
// every other value is rendered as text. A JSON value from the server can never be one.
//
// The trust check is a module-private brand, not `instanceof`: only this module's factories register a
// value, the class cannot be constructed from outside (a module-private token), so neither a subclass
// nor `Object.create(Markup.prototype)` passes, and the HTML rendered is the text stored at creation,
// not whatever `toString` the value has later (security review M1).

import { UIError } from "./errors.js";

const TOKEN: unique symbol = Symbol("erp.markup");

/** Every genuine Markup and the HTML it was created with. */
const BRAND = new WeakMap<object, string>();

/**
 * HTML marked as trusted by code. It behaves as the string it holds. Create it with `markup()` or
 * `htmlEscape()`; `new Markup(…)` is refused.
 */
export class Markup extends String {
  /**
   * Type-only (emits nothing): a private member makes `Markup` nominal, so a plain string or a `String`
   * object does not type-check as one (type review H1). The runtime check stays the brand.
   */
  private declare readonly brand: typeof TOKEN;

  constructor(token: typeof TOKEN, html: string) {
    if (token !== TOKEN) {
      throw new UIError("Markup values are created by markup() or htmlEscape(); `new Markup` is refused", "usage");
    }
    super(html);
    BRAND.set(this, html);
  }
}

function make(html: string): Markup {
  return new Markup(TOKEN, html);
}

/** Whether `value` is a Markup created by `markup()` or `htmlEscape()` (not a look-alike or subclass). */
export function isMarkup(value: unknown): value is Markup {
  return typeof value === "object" && value !== null && BRAND.has(value);
}

/** The HTML a genuine Markup was created with, or undefined for any other value (runtime only). */
export function markupHtml(value: unknown): string | undefined {
  return typeof value === "object" && value !== null ? BRAND.get(value) : undefined;
}

const ESCAPES: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  "'": "&#x27;",
  '"': "&quot;",
  "`": "&#x60;",
};

/** `value` as HTML text: a Markup as it is, anything else escaped; `undefined` is empty. */
export function htmlEscape(value: unknown): Markup {
  if (isMarkup(value)) return value;
  if (value === undefined) return make("");
  return make(String(value).replace(/[&<>'"`]/g, (c) => ESCAPES[c]!));
}

/**
 * `markup("<b>x</b>")` marks a string as HTML; a Markup is returned as it is. As a tag,
 * ``markup`<b>${v}</b>` `` escapes every interpolated value that is not itself a Markup.
 */
export function markup(value: string | TemplateStringsArray | Markup, ...values: unknown[]): Markup {
  if (isMarkup(value)) return value;
  if (typeof value === "string") return make(value);
  if (!Array.isArray(value)) {
    throw new UIError(`markup takes a string, a Markup or a template literal, got ${typeof value}`, "value");
  }
  const strings = value as TemplateStringsArray;
  let html = strings[0] ?? "";
  for (let i = 0; i < values.length; i++) {
    html += markupHtml(htmlEscape(values[i]))! + (strings[i + 1] ?? "");
  }
  return make(html);
}

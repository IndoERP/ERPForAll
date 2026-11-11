/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The DOM-writing helpers of the ABI (§8): template, insert, out,
// attr, cls, style, prop, attrs, interp. Data reaches the DOM only as `Text.data`, `setAttribute`
// values, `style.setProperty` values, class tokens or IDL properties; the two HTML sinks are
// `template` (the compiler's static, escaped HTML) and a `Markup` value in `insert`/`out`.

import { Region, blockRange, detached, endAnchor, insertBefore, removeRange } from "./block.js";
import { UIError, UnsafeAttributeError } from "./errors.js";
import { markupHtml } from "./markup.js";
import { requireOwner } from "./owner.js";
import { createEffect } from "./reactive.js";

// ---------------------------------------------------------------------------------------------------
// template

/**
 * Parses `html` once into a `<template>` and returns a cloner: `multi = 0` clones the single root
 * element, `1` a `DocumentFragment` of the roots. `html` is the compiler's serialisation of static,
 * escaped markup; the runtime never passes data here (§8 condition 1). A template holding a `<script>`,
 * also inside a nested `<template>`'s content, is refused: no template ever runs a script.
 */
export function template(html: string, multi: 0 | 1): () => Node {
  const t = document.createElement("template");
  // The only assignment of template HTML in the runtime (tests/no-html-sinks.test.ts).
  t.innerHTML = html;
  const content = document.importNode(t.content, true);
  if (holdsScript(content)) {
    throw new UIError("a template holds a <script> element; templates never run scripts", "template");
  }
  if (multi === 1) return () => content.cloneNode(true);
  const root = content.firstChild;
  if (root === null || root.nextSibling !== null || root.nodeType !== 1) {
    throw new UIError(
      `template(html, 0) needs exactly one root element, got: ${JSON.stringify(html.slice(0, 80))}`,
      "template",
    );
  }
  return () => root.cloneNode(true);
}

/** Whether `root` holds a `<script>`, looking into the content of every nested `<template>` too. */
function holdsScript(root: DocumentFragment): boolean {
  if (root.querySelector("script") !== null) return true;
  for (const t of Array.from(root.querySelectorAll("template"))) {
    if (holdsScript((t as HTMLTemplateElement).content)) return true;
  }
  return false;
}

// ---------------------------------------------------------------------------------------------------
// insert, out

/**
 * `t-out` (§7.2): `null`/`undefined` render nothing, a `Markup` its HTML, anything else one text node
 * (`String(value)`: booleans as "true"/"false", objects through their `toString`, OWL `toText`
 * owl.js:2503-2514). A text node is updated in place; a plain string is only ever assigned to
 * `Text.data`, never parsed (§8 condition 2).
 */
export function insert(parent: Node, value: () => unknown, marker: Node | null): void {
  output(parent, value, marker, null);
}

/** `t-out` with a body: as `insert`, but `null`/`undefined` render `fallback` (own owner). */
export function out(
  parent: Node,
  value: () => unknown,
  marker: Node | null,
  fallback: () => Node | null,
): void {
  output(parent, value, marker, fallback);
}

function output(
  parent: Node,
  value: () => unknown,
  marker: Node | null,
  fallback: (() => Node | null) | null,
): void {
  const owner = requireOwner("insert/out");
  const end = endAnchor(parent, marker);
  let text: Text | null = null;
  let html: string | null = null;
  let first: Node | null = null;
  let last: Node | null = null;
  let region: Region | null = null;
  const clearContent = (): void => {
    if (text !== null) {
      const t = text;
      text = null;
      // A text node code outside the runtime removed is an error, never a silent skip (review round 2,
      // silent-failure 6).
      const container = t.parentNode;
      if (container === null) throw detached();
      container.removeChild(t);
    }
    if (first !== null) {
      const f = first;
      const l = last!;
      first = last = null;
      html = null;
      removeRange(f, l);
    }
  };
  if (fallback !== null) owner.onCleanup(() => region?.dispose());
  createEffect(() => {
    const v = value();
    if (v === null || v === undefined) {
      clearContent();
      if (fallback !== null && region === null) {
        region = new Region(parent, end);
        region.build(fallback, owner);
      }
      return;
    }
    if (region !== null) {
      region.clear();
      region = null;
    }
    const h = markupHtml(v);
    if (h !== undefined) {
      if (h === html) return;
      clearContent();
      const t = document.createElement("template");
      // The one HTML sink for data: only a genuine Markup (module-private brand), which code, never
      // data, creates.
      t.innerHTML = h;
      const range = blockRange(t.content);
      if (range !== null) {
        first = range[0];
        last = range[1];
        html = h;
        insertBefore(parent, end, t.content);
      }
      return;
    }
    const s = String(v);
    if (text !== null) {
      if (text.data !== s) text.data = s;
      return;
    }
    clearContent();
    text = document.createTextNode(s);
    insertBefore(parent, end, text);
  }, owner);
}

// ---------------------------------------------------------------------------------------------------
// attr, attrs

/** Attributes whose value is a URL a browser may navigate to or load (§7.2). */
const URL_ATTRIBUTES = new Set([
  "href",
  "src",
  "action",
  "formaction",
  "poster",
  "xlink:href",
  "data",
  "codebase",
  "cite",
  "background",
  "manifest",
  "icon",
]);

/** The HTML attribute-name production (no controls, space, quotes, `>`, `/`, `=`). */
const ATTRIBUTE_NAME = /^[^\s"'>/=\u0000-\u001f\u007f-\u009f]+$/u;

function isEventAttribute(name: string): boolean {
  const c0 = name.charCodeAt(0) | 0x20;
  const c1 = name.charCodeAt(1) | 0x20;
  return c0 === 0x6f /* o */ && c1 === 0x6e /* n */;
}

/**
 * Whether a URL value runs script or embeds a non-image document: `javascript:`, `vbscript:` and every
 * `data:` URL but raster images. Browsers drop tabs and newlines anywhere and leading controls and
 * spaces before reading the scheme (URL standard), so we do too.
 */
function isScriptUrl(value: string): boolean {
  const v = value.replace(/[\t\n\r]/g, "").replace(/^[\u0000- ]+/, "").toLowerCase();
  if (v.startsWith("javascript:") || v.startsWith("vbscript:")) return true;
  if (v.startsWith("data:")) return !/^data:image\/(?!svg)[a-z0-9.+-]+[;,]/.test(v);
  return false;
}

function checkName(name: string): void {
  if (isEventAttribute(name)) {
    throw new UnsafeAttributeError(
      `refusing the attribute "${name}": a name starting with "on" is an event-handler attribute, which a template never sets dynamically (for an event, write t-on-<event>)`,
    );
  }
  if (name.toLowerCase() === "srcdoc") {
    throw new UnsafeAttributeError(`refusing a dynamic "srcdoc": it is an HTML document, never data`);
  }
}

function setAttr(el: Element, name: string, value: unknown): void {
  if (value === false || value === null || value === undefined) {
    el.removeAttribute(name);
    return;
  }
  const s = value === true ? "" : String(value);
  if (URL_ATTRIBUTES.has(name.toLowerCase()) && isScriptUrl(s)) {
    throw new UnsafeAttributeError(
      `refusing ${name}="${s.slice(0, 40)}": javascript:, vbscript: and non-image data: URLs are never set`,
    );
  }
  el.setAttribute(name, s);
}

/** ASCII-lowercased `class` or `style`, the two names with their own appliers; null for any other. */
function mergedName(name: string): "class" | "style" | null {
  if (name.length !== 5) return null;
  const lower = name.toLowerCase();
  return lower === "class" || lower === "style" ? lower : null;
}

/**
 * `t-att-<name>` / `t-attf-<name>`: `false`/`null`/`undefined` remove the attribute, `true` sets `""`,
 * anything else `setAttribute(name, String(value))`; the value is never evaluated or parsed (§8
 * condition 3). Event-handler names and script-bearing URLs are refused. `class` and `style` (any
 * case) go to the same appliers as `cls` and `style`, as in `attrs`, so a style is always written through
 * the CSSOM and never as a raw attribute (security review M3).
 */
export function attr(el: Element, name: string, value: unknown): void {
  const merged = mergedName(name);
  if (merged === "class") return cls(el, value);
  if (merged === "style") return style(el, value);
  checkName(name);
  setAttr(el, name, value);
}

interface AttrsState {
  statics: Map<string, string>;
  prev: Map<string, unknown>;
  cls: ClassState | null;
  style: StyleState | null;
}

interface ElementState {
  erpCls?: ClassState;
  erpStyle?: StyleState;
  erpAttrs?: AttrsState;
}

function attrEntries(value: unknown): Array<[string, unknown]> {
  if (value === null || value === undefined) return [];
  if (Array.isArray(value)) {
    if (value.length !== 2 || typeof value[0] !== "string") {
      throw new UIError(`t-att takes an object or a [name, value] pair, got an array of length ${value.length}`, "value");
    }
    return [[value[0], value[1]]];
  }
  if (typeof value === "object") {
    return Object.keys(value).map((k) => [k, (value as Record<string, unknown>)[k]]);
  }
  throw new UIError(`t-att takes an object or a [name, value] pair, got ${typeof value}`, "value");
}

/**
 * `t-att`: an object of attributes or a `[name, value]` pair (§7.2). Every dynamic name is checked
 * against the attribute-name production and the event-handler rule; `class` and `style` (any case)
 * merge as `cls`/`style` do; an attribute the previous value set and this one does not goes back to its
 * static value or is removed.
 */
export function attrs(el: Element, value: unknown): void {
  const holder = el as Element & ElementState;
  let state = holder.erpAttrs;
  if (state === undefined) {
    const statics = new Map<string, string>();
    for (const a of Array.from(el.attributes)) statics.set(a.name, a.value);
    state = holder.erpAttrs = { statics, prev: new Map(), cls: null, style: null };
  }
  const next = new Map<string, unknown>();
  for (const [name, v] of attrEntries(value)) {
    if (!ATTRIBUTE_NAME.test(name)) {
      throw new UnsafeAttributeError(`t-att: ${JSON.stringify(name)} is not a valid attribute name`);
    }
    checkName(name);
    next.set(name, v);
  }
  for (const [name] of state.prev) {
    if (next.has(name)) continue;
    const merged = mergedName(name);
    if (merged === "class") applyClass(el, (state.cls ??= newClassState(el)), null);
    else if (merged === "style") applyStyle(el, (state.style ??= newStyleState(el)), null);
    else if (state.statics.has(name)) el.setAttribute(name, state.statics.get(name)!);
    else el.removeAttribute(name);
  }
  for (const [name, v] of next) {
    const merged = mergedName(name);
    if (merged === "class") applyClass(el, (state.cls ??= newClassState(el)), v);
    else if (merged === "style") applyStyle(el, (state.style ??= newStyleState(el)), v);
    else if (state.prev.get(name) !== v || !state.prev.has(name)) setAttr(el, name, v);
  }
  state.prev = next;
}

/** `t-attf`: the parts concatenated, `null`/`undefined` as "" (OWL `interpolate`). */
export function interp(...parts: unknown[]): string {
  let s = "";
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (p !== null && p !== undefined) s += String(p);
  }
  return s;
}

// ---------------------------------------------------------------------------------------------------
// cls

interface ClassState {
  statics: Set<string>;
  applied: Set<string>;
}

function words(s: string, out: Set<string>): void {
  for (const w of s.split(/\s+/)) if (w !== "") out.add(w);
}

function newClassState(el: Element): ClassState {
  const statics = new Set<string>();
  words(el.getAttribute("class") ?? "", statics);
  return { statics, applied: new Set() };
}

function classWords(value: unknown): Set<string> {
  const out = new Set<string>();
  switch (typeof value) {
    case "string":
      words(value, out);
      return out;
    case "number":
    case "bigint":
      out.add(String(value));
      return out;
    case "undefined":
      return out;
    case "boolean":
      if (value) {
        throw new UIError(
          "t-att-class evaluated to true, which names no class: write `cond and 'name'` or an object {name: cond}",
          "value",
        );
      }
      return out;
    case "object":
      if (value === null) return out;
      if (Array.isArray(value)) {
        throw new UIError("t-att-class takes a string or an object {class: condition}, got an array", "value");
      }
      for (const key of Object.keys(value)) {
        if ((value as Record<string, unknown>)[key]) words(key, out);
      }
      return out;
    default:
      throw new UIError(`t-att-class takes a string or an object {class: condition}, got ${typeof value}`, "value");
  }
}

function applyClass(el: Element, state: ClassState, value: unknown): void {
  const next = classWords(value);
  const list = el.classList;
  for (const w of state.applied) {
    if (!next.has(w) && !state.statics.has(w)) list.remove(w);
  }
  for (const w of next) {
    if (!state.applied.has(w)) list.add(w);
  }
  state.applied = next;
}

/**
 * `t-att-class`: merged with the static `class` (§7.2). A string adds its words, an object each key's
 * words whose value is truthy, a number itself; `null`, `undefined`, `false` and `""` add nothing; `true`
 * is an error. A class written in the static `class` is never removed (OWL BUG-067, BUG-068).
 */
export function cls(el: Element, value: unknown): void {
  const holder = el as Element & ElementState;
  applyClass(el, (holder.erpCls ??= newClassState(el)), value);
}

// ---------------------------------------------------------------------------------------------------
// style

interface StyleState {
  statics: Map<string, string>;
  applied: Map<string, string>;
}

const IMPORTANT = /\s*!\s*important\s*$/i;
const kebabCache = new Map<string, string>();

function kebab(prop: string): string {
  if (prop.startsWith("--")) return prop;
  let k = kebabCache.get(prop);
  if (k === undefined) {
    k = prop.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase());
    kebabCache.set(prop, k);
  }
  return k;
}

/** Splits a declaration list on top-level `;` (outside quotes and parentheses). */
function parseStyle(text: string, out: Map<string, string>): void {
  const n = text.length;
  let start = 0;
  let quote = 0;
  let depth = 0;
  for (let i = 0; i <= n; i++) {
    const c = i < n ? text.charCodeAt(i) : 59;
    if (quote !== 0) {
      if (c === 92) i++;
      else if (c === quote) quote = 0;
      continue;
    }
    if (c === 34 || c === 39) quote = c;
    else if (c === 40) depth++;
    else if (c === 41 && depth > 0) depth--;
    else if (c === 59 && (depth === 0 || i === n)) {
      const part = text.slice(start, i);
      start = i + 1;
      if (part.trim() === "") continue;
      const colon = part.indexOf(":");
      const prop = colon === -1 ? "" : part.slice(0, colon).trim();
      if (prop === "") {
        // A declaration with no property name is an error, never silently dropped (review round 2,
        // silent-failure 12).
        throw new UIError(`style: ${JSON.stringify(part.trim())} is not a declaration "property: value"`, "value");
      }
      const value = part.slice(colon + 1).trim();
      if (value !== "" && value !== "undefined") out.set(prop, value);
    }
  }
}

function newStyleState(el: Element): StyleState {
  const statics = new Map<string, string>();
  parseStyle(el.getAttribute("style") ?? "", statics);
  return { statics, applied: new Map() };
}

function styleEntries(value: unknown): Map<string, string> {
  const out = new Map<string, string>();
  if (value === null || value === undefined || value === false || value === "") return out;
  if (typeof value === "string") {
    parseStyle(value, out);
    return out;
  }
  if (typeof value === "object" && !Array.isArray(value)) {
    for (const prop of Object.keys(value)) {
      const v = (value as Record<string, unknown>)[prop];
      if (v || v === 0) out.set(kebab(prop), String(v));
    }
    return out;
  }
  throw new UIError(
    `t-att-style takes a string of declarations or an object {property: value}, got ${typeof value}`,
    "value",
  );
}

function setStyleProp(style: CSSStyleDeclaration, prop: string, value: string): void {
  if (IMPORTANT.test(value)) style.setProperty(prop, value.replace(IMPORTANT, ""), "important");
  else style.setProperty(prop, value);
}

function applyStyle(el: Element, state: StyleState, value: unknown): void {
  const next = styleEntries(value);
  const style = (el as HTMLElement).style;
  for (const [prop] of state.applied) {
    if (next.has(prop)) continue;
    const s = state.statics.get(prop);
    if (s !== undefined) setStyleProp(style, prop, s);
    else style.removeProperty(prop);
  }
  for (const [prop, v] of next) {
    if (state.applied.get(prop) !== v) setStyleProp(style, prop, v);
  }
  state.applied = next;
  if (style.cssText === "") el.removeAttribute("style");
}

/**
 * `t-att-style`: merged with the static `style` (§7.2): a string of declarations or an object of
 * property → value (`0` kept, other falsy values skipped); a property the dynamic value no longer sets
 * goes back to its static value or is removed. Values go through `style.setProperty`, never parsed as
 * markup.
 */
export function style(el: Element, value: unknown): void {
  const holder = el as Element & ElementState;
  applyStyle(el, (holder.erpStyle ??= newStyleState(el)), value);
}

// ---------------------------------------------------------------------------------------------------
// prop

/** OWL `isProp` (owl.js:5968-5983): the IDL properties a template binds instead of attributes. */
function isProp(tag: string, name: string): boolean {
  switch (tag) {
    case "input":
      return (
        name === "checked" ||
        name === "indeterminate" ||
        name === "value" ||
        name === "readonly" ||
        name === "readOnly" ||
        name === "disabled"
      );
    case "option":
      return name === "selected" || name === "disabled";
    case "textarea":
      return name === "value" || name === "readonly" || name === "readOnly" || name === "disabled";
    case "select":
      return name === "value" || name === "disabled";
    case "button":
    case "optgroup":
      return name === "disabled";
    default:
      return false;
  }
}

/**
 * An IDL property of `isProp` (§7.2): `value` as a string (`0` → "0", other falsy → ""), assigned only
 * when it differs (the caret stays put), every other property as a boolean. Any other name is refused.
 */
export function prop(el: Element, name: string, value: unknown): void {
  if (!isProp(el.localName, name)) {
    throw new UnsafeAttributeError(`"${name}" is not a bindable property of <${el.localName}>`);
  }
  const target = el as unknown as Record<string, unknown>;
  if (name === "value") {
    const v = value === 0 ? "0" : value ? String(value) : "";
    if (target["value"] !== v) target["value"] = v;
    return;
  }
  const key = name === "readonly" ? "readOnly" : name;
  const b = Boolean(value);
  if (target[key] !== b) target[key] = b;
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";

/** Local admission ceilings; these bound accepted state, not native parser heap usage. */
export interface HtmlEditorLimits {
  readonly inputBytes: number;
  readonly outputBytes: number;
  readonly nodes: number;
  readonly depth: number;
  readonly historyEntries: number;
  readonly historyBytes: number;
}
const CEILINGS: Readonly<HtmlEditorLimits> = Object.freeze({ inputBytes: 1_000_000, outputBytes: 1_000_000, nodes: 10_000, depth: 64, historyEntries: 100, historyBytes: 4_000_000 });
const encoder = new TextEncoder();

/** Validate closed, positive, lower-only limit overrides. Throws ClientError validation. */
export function htmlEditorLimits(overrides?: Partial<HtmlEditorLimits>): Readonly<HtmlEditorLimits> {
  if (overrides === undefined) return Object.freeze({ ...CEILINGS });
  if (typeof overrides !== "object" || overrides === null || Array.isArray(overrides) || ![Object.prototype, null].includes(Object.getPrototypeOf(overrides))) {
    throw new ClientError("validation", "HTML editor limits must be an object.");
  }
  const result = { ...CEILINGS };
  for (const key of Reflect.ownKeys(overrides)) {
    if (typeof key !== "string" || !Object.hasOwn(CEILINGS, key)) throw new ClientError("validation", `Unknown HTML editor limit: ${String(key)}.`);
    const field = key as keyof HtmlEditorLimits;
    const value = overrides[field];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0 || value > CEILINGS[field]) {
      throw new ClientError("validation", `HTML editor limit ${field} must be a positive safe integer no greater than ${CEILINGS[field]}.`);
    }
    result[field] = value;
  }
  return Object.freeze(result);
}

const vocabulary = new Set(["p", "div", "br", "strong", "b", "em", "i", "u", "s", "blockquote", "pre", "code", "ul", "ol", "li", "h1", "h2", "h3", "h4", "h5", "h6"]);
const active = new Set(["script", "style", "iframe", "frame", "frameset", "object", "embed", "applet", "template", "title", "noscript", "form", "input", "button", "select", "textarea", "option", "link", "meta", "base", "img", "audio", "video", "source", "track", "canvas"]);
const placeholders = new WeakSet<Node>();
const transient = new WeakSet<Node>();
const transientAttributes = new Set(["contenteditable", "role", "aria-label"]);

/** Internal owner capability, never inferred from untrusted HTML attributes. */
export function markHtmlEditorPlaceholder(node: Node): void { placeholders.add(node); }
/** Internal owner capability for live editing attributes removed from exported content. */
export function markHtmlEditorTransient(node: Node): void { transient.add(node); }

type Mode = "initial" | "paste" | "export";
function unsupported(mode: Mode): never {
  throw new ClientError("unsupported", mode === "export" ? "HTML editor export contains unsupported markup." : "HTML editor initial content contains unsupported markup.");
}
function over(kind: "inputBytes" | "outputBytes" | "nodes" | "depth"): never {
  const noun = kind === "inputBytes" ? "input" : kind === "outputBytes" ? "output" : "document";
  throw new ClientError("validation", `HTML editor ${noun} exceeds ${kind} limit.`);
}
function escaped(text: string): string { return text.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;"); }

/** Reconstruct a fresh safe clone; no parsed node can become live editor content. */
function reconstruct(document: Document, source: Node, limits: HtmlEditorLimits, mode: Mode): { fragment: DocumentFragment; simplified: boolean } {
  const fragment = document.createDocumentFragment();
  const stack: { node: ChildNode; parent: Node; depth: number }[] = [];
  if (source.firstChild !== null) stack.push({ node: source.firstChild, parent: fragment, depth: 1 });
  let count = 0;
  let outputBytes = 0;
  let simplified = false;
  const charge = (value: string): void => {
    outputBytes += encoder.encode(value).length;
    if (outputBytes > limits.outputBytes) over("outputBytes");
  };
  while (stack.length > 0) {
    const frame = stack.pop()!;
    const { node, parent, depth } = frame;
    if (node.nextSibling !== null) stack.push({ node: node.nextSibling, parent, depth });
    if (++count > limits.nodes) over("nodes");
    if (depth > limits.depth) over("depth");
    if (mode === "export" && placeholders.has(node)) continue;
    if (node.nodeType === 3) {
      const text = node.nodeValue ?? ""; // DOM text nodes have a value; null denotes empty by DOM contract.
      if (text.length > limits.outputBytes - outputBytes) over("outputBytes");
      charge(escaped(text));
      parent.appendChild(document.createTextNode(text));
      continue;
    }
    if (node.nodeType !== 1) {
      if (mode !== "paste") unsupported(mode);
      simplified = true;
      continue;
    }
    const element = node as Element;
    const name = element.localName;
    if (element.namespaceURI !== "http://www.w3.org/1999/xhtml" || !vocabulary.has(name)) {
      if (mode !== "paste") unsupported(mode);
      simplified = true;
      // Active/foreign subtrees disappear; ordinary unknown wrappers lose formatting only.
      if (element.namespaceURI === "http://www.w3.org/1999/xhtml" && !active.has(name) && node.firstChild !== null) {
        stack.push({ node: node.firstChild, parent, depth: depth + 1 });
      }
      continue;
    }
    for (const attribute of element.attributes) {
      if (mode === "export" && transient.has(node) && transientAttributes.has(attribute.name) && attribute.namespaceURI === null) continue;
      if (mode !== "paste") unsupported(mode);
      simplified = true;
    }
    if (name === "br" && node.firstChild !== null) unsupported(mode);
    const canonical = name === "b" ? "strong" : name === "i" ? "em" : name;
    charge(canonical === "br" ? "<br>" : `<${canonical}></${canonical}>`);
    const copy = document.createElement(canonical);
    parent.appendChild(copy);
    if (node.firstChild !== null) stack.push({ node: node.firstChild, parent: copy, depth: depth + 1 });
  }
  return { fragment, simplified };
}

function serialize(fragment: DocumentFragment): string {
  const chunks: string[] = [];
  const stack: { node: Node; close: boolean }[] = [];
  for (let child = fragment.lastChild; child !== null; child = child.previousSibling) stack.push({ node: child, close: false });
  let scaffolding = true;
  while (stack.length > 0) {
    const { node, close } = stack.pop()!;
    if (node.nodeType === 3) {
      const text = node.nodeValue ?? "";
      if (text.length > 0) scaffolding = false;
      chunks.push(escaped(text));
      continue;
    }
    const element = node as Element;
    const name = element.localName;
    if (!["p", "div", "br"].includes(name)) scaffolding = false;
    if (close) { chunks.push(`</${name}>`); continue; }
    chunks.push(`<${name}>`);
    if (name === "br") continue;
    // The HTML parser drops one line feed right after <pre>; write it back so reading is stable.
    if (name === "pre" && node.firstChild?.nodeType === 3 && node.firstChild.nodeValue?.startsWith("\n")) chunks.push("\n");
    stack.push({ node, close: true });
    for (let child = node.lastChild; child !== null; child = child.previousSibling) stack.push({ node: child, close: false });
  }
  return scaffolding ? "" : chunks.join("");
}

/** This is the only HTML parsing sink. Template content is inert; none of its nodes is adopted. */
function parseInert(document: Document, source: string): DocumentFragment {
  const template = document.createElement("template");
  template.innerHTML = source;
  return template.content;
}

/** Refuse accepted HTML that would serialize differently after the parser reads it again
 * (nested li, heading in heading, CR, NUL...), or that initial admission would refuse by size.
 * Every accepted state can then be presented again by `admitHtml(..., "initial")`.
 * Throws ClientError unsupported (unstable) or validation (inputBytes/outputBytes). */
export function assertStableHtml(document: Document, html: string, limits: HtmlEditorLimits): void {
  const checked = htmlEditorLimits(limits);
  // Measured on the serialized string, so the line feed written back after <pre> counts too.
  const size = html.length > checked.inputBytes ? Infinity : encoder.encode(html).length;
  if (size > checked.inputBytes) over("inputBytes");
  if (size > checked.outputBytes) over("outputBytes");
  if (serialize(reconstruct(document, parseInert(document, html), checked, "initial").fragment) !== html) {
    throw new ClientError("unsupported", "HTML editor content would change when it is parsed again; the edit was refused.");
  }
}

/** Admit an ordinary untrusted string through one inert parser and fresh allowlist reconstruction. */
export function admitHtml(document: Document, source: unknown, limits: HtmlEditorLimits, mode: "initial" | "paste"): { fragment: DocumentFragment; html: string; simplified: boolean } {
  const checked = htmlEditorLimits(limits);
  if (typeof source !== "string") throw new ClientError("validation", "HTML editor value must be a string.");
  if (mode !== "initial" && mode !== "paste") throw new ClientError("validation", "HTML editor admission mode must be initial or paste.");
  if (source.length > checked.inputBytes || encoder.encode(source).length > checked.inputBytes) over("inputBytes");
  // Fragment parsing can discard document wrappers, declarations and unmatched closing tags.
  // Record unsupported raw tokens before parsing so initial admission cannot silently lose them.
  let omittedMarkup = /<!doctype\b/i.test(source);
  // EOF terminates a token too: each suffix is consumed once, including malformed unfinished tags.
  for (const token of source.matchAll(/<\/?([a-zA-Z][^\t\n\f\r />]*)(?=[\t\n\f\r />]|$)/g)) {
    if (!vocabulary.has(token[1]!.toLowerCase())) omittedMarkup = true;
    let cursor = token.index + token[0].length;
    while (cursor < source.length && /[\t\n\f\r /]/.test(source[cursor]!)) cursor++;
    if (cursor < source.length && source[cursor] !== ">") omittedMarkup = true;
  }
  if (omittedMarkup && mode === "initial") unsupported(mode);
  const result = reconstruct(document, parseInert(document, source), checked, mode);
  result.simplified ||= omittedMarkup;
  const html = serialize(result.fragment);
  if (html === "") result.fragment.replaceChildren();
  if (mode === "initial") assertStableHtml(document, html, checked);
  return { ...result, html };
}

/** Export a validated fresh clone without mutating live DOM or assigning rendering trust. */
export function exportHtml(root: HTMLElement | DocumentFragment, limits: HtmlEditorLimits): string {
  const checked = htmlEditorLimits(limits);
  const document = root.ownerDocument;
  const copy = reconstruct(document, root, checked, "export");
  return serialize(copy.fragment);
}

/** Insert literal clipboard text and normalized line breaks without using an HTML parser. */
export function plainHtml(document: Document, text: string, limits: HtmlEditorLimits): DocumentFragment {
  const checked = htmlEditorLimits(limits);
  if (typeof text !== "string") throw new ClientError("validation", "HTML editor value must be a string.");
  if (text.length > checked.inputBytes || encoder.encode(text).length > checked.inputBytes) over("inputBytes");
  const fragment = document.createDocumentFragment();
  let count = 0;
  let size = 0;
  const append = (value: string, br: boolean): void => {
    if (++count > checked.nodes) over("nodes");
    if (!br && value.length > checked.outputBytes - size) over("outputBytes");
    size += encoder.encode(br ? "<br>" : escaped(value)).length;
    if (size > checked.outputBytes) over("outputBytes");
    fragment.appendChild(br ? document.createElement("br") : document.createTextNode(value));
  };
  if (text.includes("\u0000")) throw new ClientError("validation", "HTML editor text must not contain NUL characters.");
  const normalized = text.replace(/\r\n?/g, "\n");
  let start = 0;
  for (let index = 0; index <= normalized.length; index++) {
    if (index !== normalized.length && normalized[index] !== "\n") continue;
    if (index > start) append(normalized.slice(start, index), false);
    if (index < normalized.length) append("", true);
    start = index + 1;
  }
  return fragment;
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The second layer of defence for message bodies. The server sanitizes
// first; this renderer copies only what it knows into the document and refuses oversized
// input, so a body the server let through can still not run script or load a remote resource.
import { ThreadViewError } from "./errors";

/** The largest body accepted, in UTF-16 code units (the server caps a body at 1 MiB of text). */
export const BODY_MAX_LENGTH = 1_048_576;
/** The most element and text nodes one body may produce. */
export const BODY_MAX_NODES = 5000;
/** The deepest element nesting copied. */
export const BODY_MAX_DEPTH = 32;
/** The deepest nesting walked in total, wrappers that are unwrapped included (nested tables of an email body). */
export const BODY_MAX_WALK_DEPTH = 256;
/** The class of the mention element this renderer draws (its own, never taken from the body). */
export const MENTION_CLASS = "o-mail-mention";
/** The attribute of that element carrying the mentioned partner's id. */
export const MENTION_ID_ATTRIBUTE = "data-partner-id";

const MENTION_ATTRIBUTES = ["class", "data-oe-id", "data-oe-model"];

/**
 * The partner id of a server-rendered mention anchor, or `null` when the element is anything else. The server emits
 * exactly `<a class="o_mail_redirect" data-oe-model="res.partner" data-oe-id="N">@Name</a>` (no href); an anchor with
 * another class, model, id or any extra attribute is not a mention.
 */
export function mentionPartnerId(element: Element): number | null {
  if (element.localName !== "a" || element.getAttribute("class") !== "o_mail_redirect") return null;
  if (element.getAttribute("data-oe-model") !== "res.partner") return null;
  const names = element.getAttributeNames().sort();
  if (names.length !== MENTION_ATTRIBUTES.length || names.some((n, i) => n !== MENTION_ATTRIBUTES[i])) return null;
  const id = element.getAttribute("data-oe-id") ?? "";
  if (!/^[1-9][0-9]{0,15}$/.test(id)) return null;
  const value = Number(id);
  return Number.isSafeInteger(value) ? value : null;
}

const ALLOWED = new Set(["p", "br", "b", "strong", "i", "em", "u", "s", "a", "ul", "ol", "li", "blockquote", "code", "pre", "span", "div"]);
const DROPPED_WITH_CONTENT = new Set([
  "script", "style", "iframe", "object", "embed", "form", "input", "textarea", "select", "button", "link", "meta", "template",
  "noscript", "svg", "math", "base", "title", "video", "audio", "img", "picture", "source", "canvas", "frame", "frameset",
]);
const SAFE_SCHEMES = new Set(["http:", "https:", "mailto:", "tel:"]);

/** The href kept for a link, or `null` when its scheme is not one of the safe ones (relative links are dropped too). */
export function safeHref(href: string): string | null {
  let url: URL;
  try {
    url = new URL(href);
  } catch {
    return null;
  }
  return SAFE_SCHEMES.has(url.protocol) ? url.href : null;
}

/**
 * Render a server body into a fragment of the given document.
 *
 * @param html - the body markup
 * @param doc - the document the nodes are created in
 * @throws {@link ThreadViewError} `body` when the body is over the length, node or depth cap
 */
export function renderBody(html: string, doc: Document): DocumentFragment {
  if (typeof html !== "string") throw new ThreadViewError("body", `a message body must be text, got ${typeof html}`);
  if (html.length > BODY_MAX_LENGTH) {
    throw new ThreadViewError("body", `message body is ${html.length} characters; the display limit is ${BODY_MAX_LENGTH}`);
  }
  const parsed = new DOMParser().parseFromString(html, "text/html");
  const out = doc.createDocumentFragment();
  let nodes = 0;
  const count = (): void => {
    nodes += 1;
    if (nodes > BODY_MAX_NODES) throw new ThreadViewError("body", `message body has more than ${BODY_MAX_NODES} nodes; the display limit is ${BODY_MAX_NODES}`);
  };
  const copy = (source: Node, target: Node, depth: number, walk: number): void => {
    for (const child of Array.from(source.childNodes)) {
      if (child.nodeType === 3) {
        count();
        target.appendChild(doc.createTextNode(child.nodeValue ?? ""));
        continue;
      }
      if (child.nodeType !== 1) continue;
      const element = child as Element;
      const tag = element.localName;
      if (DROPPED_WITH_CONTENT.has(tag)) continue;
      if (walk >= BODY_MAX_WALK_DEPTH) throw new ThreadViewError("body", `message body nests deeper than ${BODY_MAX_WALK_DEPTH} elements in total; the display limit is ${BODY_MAX_WALK_DEPTH}`);
      if (!ALLOWED.has(tag)) {
        copy(element, target, depth, walk + 1);
        continue;
      }
      if (depth >= BODY_MAX_DEPTH) throw new ThreadViewError("body", `message body nests deeper than ${BODY_MAX_DEPTH} elements; the display limit is ${BODY_MAX_DEPTH}`);
      count();
      const partner = mentionPartnerId(element);
      if (partner !== null) {
        const mention = doc.createElement("span");
        mention.setAttribute("class", MENTION_CLASS);
        mention.setAttribute(MENTION_ID_ATTRIBUTE, String(partner));
        copy(element, mention, depth + 1, walk + 1);
        target.appendChild(mention);
        continue;
      }
      const node = doc.createElement(tag);
      if (tag === "a") {
        const href = element.getAttribute("href");
        const safe = href === null ? null : safeHref(href);
        if (safe !== null) {
          node.setAttribute("href", safe);
          node.setAttribute("rel", "noopener noreferrer");
          node.setAttribute("target", "_blank");
        }
      }
      copy(element, node, depth + 1, walk + 1);
      target.appendChild(node);
    }
  };
  copy(parsed.body, out, 0, 0);
  return out;
}

/**
 * The text of a body without markup, with a newline after each block (for copying and for descriptions).
 *
 * @param html - the body markup
 * @param doc - the document the nodes are created in
 */
export function bodyText(html: string, doc: Document): string {
  const holder = doc.createElement("div");
  holder.appendChild(renderBody(html, doc));
  const parts: string[] = [];
  const walk = (node: Node): void => {
    for (const child of Array.from(node.childNodes)) {
      if (child.nodeType === 3) parts.push(child.nodeValue ?? "");
      else if (child.nodeType === 1) {
        const tag = (child as Element).localName;
        if (tag === "br") parts.push("\n");
        else {
          walk(child);
          if (tag === "p" || tag === "div" || tag === "li" || tag === "blockquote" || tag === "pre") parts.push("\n");
        }
      }
    }
  };
  walk(holder);
  return parts.join("").replace(/\n{3,}/g, "\n\n").trim();
}

const plain = new WeakMap<object, string>();

/**
 * {@link bodyText} of a message body, computed once per message snapshot (a render asks several times).
 *
 * @param owner - the snapshot the body belongs to (the cache key)
 * @param html - its body
 * @param doc - the document the nodes are created in
 */
export function bodyTextOf(owner: object, html: string, doc: Document): string {
  let text = plain.get(owner);
  if (text === undefined) {
    text = bodyText(html, doc);
    plain.set(owner, text);
  }
  return text;
}

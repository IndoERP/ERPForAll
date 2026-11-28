/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import DOMPurify from "dompurify";
import { Marked } from "marked";

const MAX_REPLY_LENGTH = 65_536;
const MAX_ELEMENTS = 5_000;
const formattingAttributes = ["align", "scope", "rowspan", "colspan", "start", "lang"];
const tags = [
  "p", "br", "hr", "h1", "h2", "h3", "h4", "h5", "h6", "strong", "b", "em", "i", "u", "s", "del",
  "ul", "ol", "li", "blockquote", "pre", "code", "a", "div", "span", "sub", "sup",
  "table", "caption", "thead", "tbody", "tfoot", "tr", "th", "td",
];

function escapeText(value: string): string {
  return value.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;");
}

const markdown = new Marked({
  gfm: true, breaks: true,
  renderer: {
    html({ text, block }) {
      // Keep a literal tag in prose readable (e.g. “<img> is text”). Whole HTML blocks
      // and supported inline elements pass through the same sanitizer as Markdown.
      const tag = !block && /^<\/?([a-z][\w-]*)\b/i.exec(text);
      return tag && !tags.includes(tag[1]!.toLowerCase()) ? escapeText(text) : false;
    },
    code({ text, lang }) {
      const language = lang?.trim().toLowerCase();
      const preview = language === "html" || language === "md" || language === "markdown";
      return '<pre><code' + (preview ? ' lang="' + language + '"' : "") + '>' + escapeText(text) + '</code></pre>\n';
    },
  },
});

function clean(html: string): DocumentFragment {
  const fragment = DOMPurify.sanitize(html, {
    RETURN_DOM_FRAGMENT: true, ALLOWED_TAGS: tags,
    ALLOWED_ATTR: ["href", ...formattingAttributes], ADD_URI_SAFE_ATTR: formattingAttributes,
    ALLOW_DATA_ATTR: false, ALLOW_ARIA_ATTR: false,
    ALLOWED_URI_REGEXP: /^(?:https?:\/\/|mailto:)/i,
  });
  for (const link of fragment.querySelectorAll("a[href]")) {
    link.setAttribute("target", "_blank");
    link.setAttribute("rel", "noopener noreferrer");
  }
  return fragment;
}

function previewTable(language: string, source: string): HTMLTableElement | undefined {
  let html: string;
  if (language === "html") {
    if (!/^<table(?:\s|>)/i.test(source.trimStart()) || !/<\/table>$/i.test(source.trimEnd())) return;
    html = source;
  } else {
    const blocks = markdown.lexer(source).filter(token => token.type !== "space");
    if (blocks.length !== 1 || blocks[0]?.type !== "table") return;
    html = markdown.parser(blocks);
  }
  const fragment = clean(html);
  const roots = Array.from(fragment.childNodes).filter(node => node.nodeType !== Node.TEXT_NODE || node.textContent?.trim());
  const table = fragment.querySelector("table");
  return table && roots.length === 1 && roots[0] === table && table.rows[0]?.cells.length ? table : undefined;
}

function plainReply(container: HTMLElement, text: string, reason: string) {
  const notice = document.createElement("small");
  notice.className = "ai-reply-notice";
  notice.textContent = reason;
  container.replaceChildren(notice, document.createTextNode(text));
}

// GFM pads a short row to its header width while tokenizing. Bound that expansion before
// passing a sparse table to Marked, including one inside a fenced preview or a blockquote.
function fitsFormattingBudget(text: string): boolean {
  if (text.length > MAX_REPLY_LENGTH) return false;
  let width = 0, lines = 0;
  for (const line of text.split(/\r\n|[\r\n]/)) {
    if (!line.trim()) { width = 0; lines = 0; continue; }
    lines++;
    if (line.includes("|") && /^[\s>|:-]+$/.test(line)) {
      width = Math.max(width, line.split("|").length);
    }
    if (width * lines > MAX_ELEMENTS) return false;
  }
  return true;
}

/** Format prose only. Session storage, ERP views and operation controls consume the original reply. */
export function renderConversationText(container: HTMLElement, text: string): void {
  if (!fitsFormattingBudget(text)) {
    plainReply(container, text, "Long reply shown as plain text.");
    return;
  }
  try {
    const body = clean(markdown.parse(text, { async: false }));
    for (const code of body.querySelectorAll("pre > code[lang]")) {
      const language = code.getAttribute("lang");
      if (language !== "html" && language !== "md" && language !== "markdown") continue;
      code.removeAttribute("lang");
      const table = previewTable(language, code.textContent ?? "");
      if (!table) continue;
      const pre = code.parentElement!;
      const details = document.createElement("details");
      const summary = document.createElement("summary");
      summary.textContent = "Lihat sumber";
      pre.replaceWith(table, details);
      details.append(summary, pre);
    }
    if (body.querySelectorAll("*").length > MAX_ELEMENTS) {
      plainReply(container, text, "Long reply shown as plain text.");
      return;
    }
    for (const table of body.querySelectorAll("table")) {
      for (const header of table.tHead?.querySelectorAll<HTMLTableCellElement>("th:not([scope])") ?? []) {
        header.scope = "col";
      }
      const scroll = document.createElement("div");
      scroll.className = "ai-reply-table";
      scroll.tabIndex = 0;
      scroll.setAttribute("role", "region");
      scroll.setAttribute("aria-label", table.caption?.textContent?.trim() || "Conversation table");
      table.replaceWith(scroll);
      scroll.append(table);
    }
    const prose = document.createElement("div");
    prose.className = "ai-reply-prose";
    prose.append(body);
    container.replaceChildren(prose);
  } catch {
    plainReply(container, text, "Formatting unavailable. Showing the original reply.");
  }
}

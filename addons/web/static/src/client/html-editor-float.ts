/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Pure rules for the HTML editor's floating surfaces: where the toolbar and the slash list sit, when the
// toolbar shows, how typed text selects a slash command, and how a command rewrites the caret's block.

export interface Box { left: number; top: number; width: number; height: number }

/** A floating box above its anchor (below when there is no room above), clamped into the viewport.
 *  Null when the anchor itself is scrolled out of view: the surface hides rather than hang over other content. */
export function placeFloating(anchor: Box, size: {width: number; height: number}, viewport: {width: number; height: number}, gap = 8, margin = 8): {left: number; top: number; side: "above" | "below"} | null {
  if (anchor.top + anchor.height < 0 || anchor.top > viewport.height) return null;
  const above = anchor.top - gap - size.height;
  const side = above >= margin ? "above" : "below";
  const wanted = side === "above" ? above : anchor.top + anchor.height + gap;
  const clamp = (value: number, extent: number, limit: number): number => Math.max(margin, Math.min(value, limit - extent - margin));
  return {left: clamp(anchor.left, size.width, viewport.width), top: clamp(wanted, size.height, viewport.height), side};
}

/** The formatting toolbar belongs to a non-collapsed selection of visible text inside an editable editor. */
export function toolbarVisible(state: {inside: boolean; collapsed: boolean; text: string; composing: boolean; locked: boolean; pointerDown: boolean; dismissed: boolean}): boolean {
  return state.inside && !state.collapsed && !state.composing && !state.locked && !state.pointerDown && !state.dismissed && /[^\s​﻿]/.test(state.text);
}

export interface SlashCommand { id: string; title: string; description: string; keywords: readonly string[] }

/** `/` at the start of a text run or after a space opens the list; the term runs to the caret and holds no space. */
export function slashTrigger(textBeforeCaret: string): {term: string; start: number} | null {
  const match = /(?:^|\s)\/([^\s/]*)$/.exec(textBeforeCaret);
  return match ? {term: match[1]!, start: textBeforeCaret.length - match[1]!.length - 1} : null;
}

/** Title prefix first, then word prefix, then substring of the title or a keyword; original order breaks ties. */
export function filterCommands<T extends SlashCommand>(commands: readonly T[], term: string): T[] {
  const needle = term.trim().toLowerCase();
  if (!needle) return [...commands];
  const rank = (command: T): number => {
    const title = command.title.toLowerCase();
    if (title.startsWith(needle)) return 0;
    if (title.split(/\s+/).some(word => word.startsWith(needle)) || command.keywords.some(word => word.startsWith(needle))) return 1;
    return title.includes(needle) || command.keywords.some(word => word.includes(needle)) ? 2 : 3;
  };
  return commands.map((command, order) => ({command, order, score: rank(command)})).filter(item => item.score < 3)
    .sort((a, b) => a.score - b.score || a.order - b.order).map(item => item.command);
}

export type BlockKind = "p" | "h1" | "h2" | "h3" | "ul" | "ol" | "blockquote" | "pre";
/** Every command the list offers is a block the editor already admits (see the vocabulary in html-editor-document). */
export const BLOCK_COMMANDS: readonly (SlashCommand & {block: BlockKind})[] = [
  {id: "paragraph", block: "p", title: "Text", description: "Plain paragraph", keywords: ["paragraph", "normal", "p"]},
  {id: "heading1", block: "h1", title: "Heading 1", description: "Large section title", keywords: ["h1", "title", "header"]},
  {id: "heading2", block: "h2", title: "Heading 2", description: "Medium section title", keywords: ["h2", "subtitle", "header"]},
  {id: "heading3", block: "h3", title: "Heading 3", description: "Small section title", keywords: ["h3", "header"]},
  {id: "bullets", block: "ul", title: "Bulleted list", description: "Unordered list", keywords: ["list", "ul", "bullet"]},
  {id: "numbers", block: "ol", title: "Numbered list", description: "Ordered list", keywords: ["list", "ol", "number"]},
  {id: "quote", block: "blockquote", title: "Quote", description: "Indented quotation", keywords: ["blockquote", "citation"]},
  {id: "code", block: "pre", title: "Code block", description: "Preformatted text", keywords: ["pre", "monospace"]},
];

const BLOCK_SELECTOR = "p,div,blockquote,pre,ul,ol,li,h1,h2,h3,h4,h5,h6";
const TEXT_BLOCKS = new Set(["p", "div", "pre", "h1", "h2", "h3", "h4", "h5", "h6"]);

/** The top-level child of root holding the node, or null when that child is a list or quote (nested blocks are not rewritten). */
function topChild(root: HTMLElement, node: Node): ChildNode | null {
  let child: Node = node;
  while (child.parentNode && child.parentNode !== root) child = child.parentNode;
  if (child.parentNode !== root) return null;
  if (child instanceof HTMLElement && child.matches(BLOCK_SELECTOR)) return TEXT_BLOCKS.has(child.localName) && !child.querySelector(BLOCK_SELECTOR) ? child as ChildNode : null;
  return child as ChildNode;
}

/** Whether the caret sits in a block the slash commands may rewrite. */
export function blockAvailable(root: HTMLElement, node: Node): boolean { return topChild(root, node) !== null; }

/** Rewrite the caret's top-level block as `kind`, keeping its content. Returns the new block. Inline text directly
 *  under the root is first gathered into a paragraph. Throws no error: callers check `blockAvailable` first. */
export function convertBlock(root: HTMLElement, node: Node, kind: BlockKind): HTMLElement {
  const owner = root.ownerDocument, child = topChild(root, node)!;
  let host: HTMLElement;
  if (child instanceof HTMLElement && TEXT_BLOCKS.has(child.localName)) host = child;
  else {
    host = owner.createElement("p");
    const run: ChildNode[] = [child];
    for (let next = child.previousSibling; next && !(next instanceof HTMLElement && next.matches(BLOCK_SELECTOR)); next = next.previousSibling) run.unshift(next);
    for (let next = child.nextSibling; next && !(next instanceof HTMLElement && next.matches(BLOCK_SELECTOR)); next = next.nextSibling) run.push(next);
    run[0]!.before(host); host.append(...run);
  }
  if (!host.firstChild) host.append(owner.createElement("br"));
  const inner = (tag: string): HTMLElement => { const element = owner.createElement(tag); element.append(...host.childNodes); return element; };
  let result: HTMLElement;
  if (kind === "ul" || kind === "ol") { result = owner.createElement(kind); result.append(inner("li")); }
  else if (kind === "blockquote") { result = owner.createElement(kind); result.append(inner("p")); }
  else if (host.localName === kind) return host;
  else result = inner(kind);
  host.replaceWith(result);
  return result;
}

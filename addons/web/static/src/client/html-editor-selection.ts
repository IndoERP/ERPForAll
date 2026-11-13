/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/** A selection address within one editable owner, including its direction. */
export interface SelectionBookmark { anchor: number[]; anchorOffset: number; focus: number[]; focusOffset: number }

function protectedNode(root: HTMLElement, node: Node): boolean {
  let element = node.nodeType === 1 ? node as Element : node.parentElement;
  while (element) {
    if (element.getAttribute("contenteditable") === "false") return true;
    if (element === root) break;
    element = element.parentElement;
  }
  return false;
}

/** Only a connected, wholly owned and editable selection authorizes a command. */
export function editableRange(root: HTMLElement): Range | null {
  const selection = root.ownerDocument.getSelection();
  if (!root.isConnected || !selection || selection.rangeCount !== 1 || !selection.anchorNode || !selection.focusNode ||
      !root.contains(selection.anchorNode) || !root.contains(selection.focusNode) ||
      protectedNode(root, selection.anchorNode) || protectedNode(root, selection.focusNode)) return null;
  const range = selection.getRangeAt(0);
  for (const island of root.querySelectorAll('[contenteditable="false"]')) {
    if (range.intersectsNode(island)) return null;
  }
  return range;
}

function address(root: HTMLElement, node: Node): number[] {
  const path: number[] = [];
  while (node !== root && node.parentNode) {
    const parent = node.parentNode;
    path.unshift(Array.prototype.indexOf.call(parent.childNodes, node));
    node = parent;
  }
  return path;
}

export function captureSelection(root: HTMLElement): SelectionBookmark | null {
  if (!editableRange(root)) return null;
  const selection = root.ownerDocument.getSelection()!;
  return {anchor: address(root, selection.anchorNode!), anchorOffset: selection.anchorOffset,
    focus: address(root, selection.focusNode!), focusOffset: selection.focusOffset};
}

function resolve(root: HTMLElement, path: readonly number[], offset: number): Node | null {
  let node: Node = root;
  for (const index of path) {
    if (!Number.isSafeInteger(index) || index < 0 || index >= node.childNodes.length) return null;
    node = node.childNodes[index]!;
  }
  const length = node.nodeType === 3 ? node.textContent!.length : node.childNodes.length;
  return Number.isSafeInteger(offset) && offset >= 0 && offset <= length && !protectedNode(root, node) ? node : null;
}

/** A stale bookmark is refused without moving another owner's selection. */
export function restoreSelection(root: HTMLElement, bookmark: SelectionBookmark): void {
  if (!root.isConnected) return;
  const anchor = resolve(root, bookmark.anchor, bookmark.anchorOffset), focus = resolve(root, bookmark.focus, bookmark.focusOffset);
  if (!anchor || !focus) return;
  const range = root.ownerDocument.createRange();
  range.setStart(anchor, bookmark.anchorOffset); range.collapse(true);
  const other = root.ownerDocument.createRange(); other.setStart(focus, bookmark.focusOffset); other.collapse(true);
  if (range.compareBoundaryPoints(Range.START_TO_START, other) <= 0) range.setEnd(focus, bookmark.focusOffset);
  else { range.setStart(focus, bookmark.focusOffset); range.setEnd(anchor, bookmark.anchorOffset); }
  for (const island of root.querySelectorAll('[contenteditable="false"]')) if (range.intersectsNode(island)) return;
  root.ownerDocument.getSelection()?.setBaseAndExtent(anchor, bookmark.anchorOffset, focus, bookmark.focusOffset);
}

/** Inline formatting the editor can write; each maps to tags the server sanitizer keeps. `tag` is what a toggle creates. */
export const MARKS = {
  bold: {names: ["strong", "b"], tag: "strong"},
  italic: {names: ["em", "i"], tag: "em"},
  underline: {names: ["u"], tag: "u"},
  strike: {names: ["s"], tag: "s"},
} as const;
export type MarkName = keyof typeof MARKS;
const MARK_TAGS: ReadonlySet<string> = new Set(Object.values(MARKS).flatMap(mark => mark.names));

function markAncestor(root: HTMLElement, node: Node, names: readonly string[]): HTMLElement | null {
  for (let parent = node.parentElement; parent && parent !== root; parent = parent.parentElement) {
    if (names.includes(parent.localName)) return parent;
  }
  return null;
}

/** True when the selection holds text and every selected text run sits inside the mark. */
export function rangeHasMark(root: HTMLElement, mark: MarkName): boolean {
  const range = editableRange(root);
  if (!range || range.collapsed) return false;
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let found = false;
  for (let text = walker.nextNode(); text; text = walker.nextNode()) {
    const from = text === range.startContainer ? range.startOffset : 0, to = text === range.endContainer ? range.endOffset : (text.textContent ?? "").length;
    if (!range.intersectsNode(text) || to <= from) continue;
    if (!markAncestor(root, text, MARKS[mark].names)) return false;
    found = true;
  }
  return found;
}

function position(root: HTMLElement, node: Node, offset: number): number {
  const prefix = root.ownerDocument.createRange(); prefix.selectNodeContents(root); prefix.setEnd(node, offset); return prefix.toString().length;
}

function textPosition(root: HTMLElement, offset: number, preferNext = false): {node: Node; offset: number} {
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode(), last: Node | null = null;
  while (node) {
    const length = node.textContent!.length;
    if (offset < length || offset === length && !preferNext) return {node, offset};
    offset -= length; last = node; node = walker.nextNode();
  }
  return last ? {node: last, offset: last.textContent!.length} : {node: root, offset: 0};
}

function removeMark(root: HTMLElement, text: Text, names: readonly string[]): void {
  let ancestor = markAncestor(root, text, names);
  while (ancestor) {
    const left = root.ownerDocument.createRange(), right = root.ownerDocument.createRange();
    left.selectNodeContents(ancestor); left.setEndBefore(text);
    right.selectNodeContents(ancestor); right.setStartAfter(text);
    // Extract whole untouched branches so protected islands retain their node identity and
    // owner capability. Range clones only partially selected inline ancestors.
    const leftContent = left.extractContents(), rightContent = right.extractContents();
    let middle: Node = text;
    for (let parent = text.parentElement; parent && parent !== ancestor; parent = parent.parentElement) {
      const wrapper = parent.cloneNode(false); wrapper.appendChild(middle); middle = wrapper;
    }
    const replacement = root.ownerDocument.createDocumentFragment();
    if (leftContent.textContent || leftContent.querySelector("br")) { const wrapper = ancestor.cloneNode(false); wrapper.appendChild(leftContent); replacement.append(wrapper); }
    replacement.append(middle);
    if (rightContent.textContent || rightContent.querySelector("br")) { const wrapper = ancestor.cloneNode(false); wrapper.appendChild(rightContent); replacement.append(wrapper); }
    ancestor.replaceWith(replacement); ancestor = markAncestor(root, text, names);
  }
}

function normalize(root: HTMLElement): void {
  // All admitted trees are bounded by document admission; use an iterative postorder walk.
  const elements = [root, ...root.querySelectorAll<HTMLElement>("*")].reverse();
  for (const element of elements) {
    if (protectedNode(root, element)) continue;
    let child = element.firstChild;
    while (child) {
      const next = child.nextSibling;
      if (child instanceof Text && next instanceof Text) { child.appendData(next.data); next.remove(); continue; }
      if (child instanceof HTMLElement && next instanceof HTMLElement && MARK_TAGS.has(child.localName) && child.localName === next.localName && child.attributes.length === 0 && next.attributes.length === 0) {
        child.append(...next.childNodes); next.remove(); continue;
      }
      child = next;
    }
  }
}

/** Toggle only selected text runs. No native execCommand or browser undo authority. */
export function toggleBold(root: HTMLElement): boolean { return toggleMark(root, "bold"); }

export function toggleMark(root: HTMLElement, markName: MarkName): boolean {
  const mark = MARKS[markName];
  const range = editableRange(root);
  if (!range || range.collapsed) return false;
  const selection = root.ownerDocument.getSelection()!;
  const anchor = position(root, selection.anchorNode!, selection.anchorOffset), focus = position(root, selection.focusNode!, selection.focusOffset);
  const runs: {start: number; end: number; bold: boolean}[] = [];
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let current = walker.nextNode(), offset = 0;
  while (current) {
    const text = current as Text;
    if (range.intersectsNode(text)) {
      const start = text === range.startContainer ? range.startOffset : 0;
      const end = text === range.endContainer ? range.endOffset : text.length;
      if (end > start) runs.push({start: offset + start, end: offset + end, bold: markAncestor(root, text, mark.names) !== null});
    }
    offset += text.length;
    current = walker.nextNode();
  }
  if (!runs.length) return false;
  const unbold = runs.every(run => run.bold);
  for (const run of runs) {
    // Removing a bold ancestor splits/clones its other branches. Resolve each staged text
    // interval against the current tree rather than retaining nodes from a removed ancestor.
    const point = textPosition(root, run.start, true), text = point.node as Text, end = point.offset + run.end - run.start;
    if (end < text.length) text.splitText(end);
    const selected = point.offset > 0 ? text.splitText(point.offset) : text;
    if (unbold) removeMark(root, selected, mark.names);
    else if (!markAncestor(root, selected, mark.names)) { const wrapper = root.ownerDocument.createElement(mark.tag); selected.replaceWith(wrapper); wrapper.append(selected); }
  }
  normalize(root);
  const first = textPosition(root, anchor), last = textPosition(root, focus);
  selection.setBaseAndExtent(first.node, first.offset, last.node, last.offset);
  return true;
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createHtmlEditor } from "../src/client/html-editor.js";
import { BLOCK_COMMANDS, blockAvailable, convertBlock, filterCommands, placeFloating, slashTrigger, toolbarVisible } from "../src/client/html-editor-float.js";
import { toggleMark } from "../src/client/html-editor-selection.js";
import { MessageServices } from "../src/client/messages.js";

const disposers: (() => void)[] = [];
afterEach(() => { for (const dispose of disposers.splice(0)) dispose(); document.getSelection()?.removeAllRanges(); document.body.replaceChildren(); });
function setup(value = "<p>Hello world</p>") {
  const change = vi.fn(), messages = new MessageServices(document.body, vi.fn());
  const editor = createHtmlEditor({environment: {messages, report: vi.fn()}, label: "Notes", value, change});
  document.body.append(editor.node); disposers.push(() => editor.dispose(), () => messages.dispose());
  return {editor, change};
}
function select(root: HTMLElement, start: Node, from: number, end: Node = start, to = from): void {
  root.focus(); document.getSelection()!.setBaseAndExtent(start, from, end, to); document.dispatchEvent(new Event("selectionchange"));
}
function type(root: HTMLElement, text: string): void {
  root.focus(); root.dispatchEvent(new InputEvent("beforeinput", {bubbles: true, cancelable: true, inputType: "insertText", data: text.slice(-1)}));
  root.textContent = text; select(root, root.firstChild!, text.length); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: text})); }
function press(root: HTMLElement, key: string, init: KeyboardEventInit = {}): KeyboardEvent {
  const event = new KeyboardEvent("keydown", {key, bubbles: true, cancelable: true, ...init}); root.dispatchEvent(event); return event;
}
const toolbarOf = (root: HTMLElement): HTMLElement => root.querySelector<HTMLElement>(".html-editor-toolbar")!;
const menuOf = (root: HTMLElement): HTMLElement => root.querySelector<HTMLElement>(".html-slash-menu")!;
const viewport = {width: 800, height: 600};

test("placement puts the toolbar above its anchor, flips below at the top edge and stays inside the viewport", () => {
  const size = {width: 200, height: 40};
  expect(placeFloating({left: 100, top: 300, width: 50, height: 20}, size, viewport)).toEqual({left: 100, top: 252, side: "above"});
  expect(placeFloating({left: 100, top: 20, width: 50, height: 20}, size, viewport)).toEqual({left: 100, top: 48, side: "below"});
  expect(placeFloating({left: 780, top: 300, width: 50, height: 20}, size, viewport)?.left).toBe(592);
  expect(placeFloating({left: -50, top: 300, width: 50, height: 20}, size, viewport)?.left).toBe(8);
  expect(placeFloating({left: 100, top: 590, width: 50, height: 40}, size, viewport)?.top).toBeLessThanOrEqual(600 - 40 - 8);
});
test("placement hides the surface when its anchor is scrolled out of the viewport", () => {
  expect(placeFloating({left: 10, top: -80, width: 50, height: 20}, {width: 100, height: 30}, viewport)).toBeNull();
  expect(placeFloating({left: 10, top: 700, width: 50, height: 20}, {width: 100, height: 30}, viewport)).toBeNull();
});
test("the toolbar shows only for a non-collapsed selection of visible text in an editable editor", () => {
  const base = {inside: true, collapsed: false, text: "word", composing: false, locked: false, pointerDown: false, dismissed: false};
  expect(toolbarVisible(base)).toBe(true);
  for (const change of [{inside: false}, {collapsed: true}, {text: " \n​"}, {composing: true}, {locked: true}, {pointerDown: true}, {dismissed: true}]) expect(toolbarVisible({...base, ...change})).toBe(false);
});
test("a slash opens the list at the start of text or after a space, never inside a word or after a space in the term", () => {
  expect(slashTrigger("/")).toEqual({term: "", start: 0});
  expect(slashTrigger("Hello /hea")).toEqual({term: "hea", start: 6});
  expect(slashTrigger("a/b")).toBeNull(); expect(slashTrigger("/head ")).toBeNull(); expect(slashTrigger("plain")).toBeNull();
});
test("command filtering ranks title prefix before keyword and substring, and returns nothing for no match", () => {
  expect(filterCommands(BLOCK_COMMANDS, "").length).toBe(BLOCK_COMMANDS.length);
  expect(filterCommands(BLOCK_COMMANDS, "head").map(command => command.id)).toEqual(["heading1", "heading2", "heading3"]);
  expect(filterCommands(BLOCK_COMMANDS, "LIST").map(command => command.id)).toEqual(["bullets", "numbers"]);
  expect(filterCommands(BLOCK_COMMANDS, "h2")[0]?.id).toBe("heading2");
  expect(filterCommands(BLOCK_COMMANDS, "zzz")).toEqual([]);
});
test("convertBlock rewrites the caret block, gathers bare inline text and refuses nested containers", () => {
  const root = document.createElement("div"); root.innerHTML = "<p>One</p>bare <strong>text</strong><ul><li>item</li></ul>";
  expect(convertBlock(root, root.firstChild!.firstChild!, "h2").outerHTML).toBe("<h2>One</h2>");
  convertBlock(root, root.childNodes[1]!, "ol");
  expect(root.innerHTML).toBe("<h2>One</h2><ol><li>bare <strong>text</strong></li></ol><ul><li>item</li></ul>");
  expect(blockAvailable(root, root.querySelector("ul li")!.firstChild!)).toBe(false);
  const quote = document.createElement("div"); quote.innerHTML = "<p></p>"; convertBlock(quote, quote.firstChild!, "blockquote");
  expect(quote.innerHTML).toBe("<blockquote><p><br></p></blockquote>");
});
test("toggleMark writes italic, underline and strikethrough inside the admitted tags and removes them again", () => {
  for (const [mark, tag] of [["italic", "em"], ["underline", "u"], ["strike", "s"]] as const) {
    const {editor} = setup("<p>Hello world</p>"), text = editor.editable.firstChild!.firstChild!;
    select(editor.editable, text, 0, text, 5);
    expect(toggleMark(editor.editable, mark)).toBe(true); expect(editor.editable.innerHTML).toBe(`<p><${tag}>Hello</${tag}> world</p>`);
    expect(toggleMark(editor.editable, mark)).toBe(true); expect(editor.editable.innerHTML).toBe("<p>Hello world</p>");
  }
});
test("the field carries a hint when empty and no permanent strip: the toolbar is hidden until text is selected", () => {
  const {editor} = setup("");
  expect(editor.editable.dataset["empty"]).toBe("true"); expect(editor.editable.dataset["hint"]).toBe('Type "/" for commands');
  expect(toolbarOf(editor.node).hidden).toBe(true); expect(toolbarOf(editor.node).getAttribute("role")).toBe("toolbar");
  type(editor.editable, "typed"); expect(editor.editable.dataset["empty"]).toBe("false");
});
test("the toolbar appears over a selection, applies italic, and hides on collapse and on Escape", () => {
  const {editor, change} = setup(), root = editor.editable, text = root.firstChild!.firstChild!, toolbar = toolbarOf(editor.node);
  select(root, text, 0, text, 5); expect(toolbar.hidden).toBe(false);
  toolbar.querySelector<HTMLButtonElement>('[data-html-command="italic"]')!.click();
  expect(editor.exportHtml()).toBe("<p><em>Hello</em> world</p>"); expect(change).toHaveBeenCalledWith("<p><em>Hello</em> world</p>");
  expect(toolbar.querySelector('[data-html-command="italic"]')!.getAttribute("aria-pressed")).toBe("true");
  const first = root.querySelector("em")!.firstChild!; select(root, first, 2);
  expect(toolbar.hidden).toBe(true);
  select(root, first, 0, first, 3); expect(toolbar.hidden).toBe(false);
  expect(press(root, "Escape").defaultPrevented).toBe(true); expect(toolbar.hidden).toBe(true);
  select(root, first, 0, first, 2); expect(toolbar.hidden).toBe(false);
});
test("toolbar keyboard: Alt+F focuses it, arrows move between buttons, Escape returns to the text", () => {
  const {editor} = setup(), root = editor.editable, text = root.firstChild!.firstChild!, toolbar = toolbarOf(editor.node);
  select(root, text, 0, text, 5);
  press(root, "ƒ", {altKey: true, code: "KeyF"});
  const buttons = [...toolbar.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
  expect(document.activeElement).toBe(buttons[0]);
  press(toolbar, "ArrowRight"); expect(document.activeElement).toBe(buttons[1]);
  press(toolbar, "ArrowLeft"); press(toolbar, "ArrowLeft"); expect(document.activeElement).toBe(buttons.at(-1));
  press(document.activeElement as HTMLElement, "Escape"); expect(document.activeElement).toBe(root);
});
test("typing a slash opens the list, filters while typing, moves with arrows and applies a block with Enter", () => {
  const {editor, change} = setup(""), root = editor.editable, menu = menuOf(editor.node);
  type(root, "/"); expect(menu.hidden).toBe(false); expect(menu.querySelectorAll('[role="option"]').length).toBe(BLOCK_COMMANDS.length);
  expect(root.getAttribute("aria-activedescendant")).toBe(menu.querySelector('[aria-selected="true"]')!.id);
  type(root, "/head"); expect([...menu.querySelectorAll(".html-slash-title")].map(node => node.textContent)).toEqual(["Heading 1", "Heading 2", "Heading 3"]);
  expect(press(root, "ArrowDown").defaultPrevented).toBe(true);
  expect(menu.querySelectorAll('[role="option"]')[1]!.getAttribute("aria-selected")).toBe("true");
  expect(press(root, "Enter").defaultPrevented).toBe(true);
  expect(editor.exportHtml()).toBe("<h2><br></h2>"); expect(menu.hidden).toBe(true); expect(root.hasAttribute("aria-activedescendant")).toBe(false);
  expect(change).toHaveBeenLastCalledWith("<h2><br></h2>");
});
function typeInto(root: HTMLElement, text: Text, value: string): void {
  text.data = value; select(root, text, value.length); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: value.slice(-1)}));
}
test("the slash list keeps the text before it, closes on Escape, on no match and on a space, and reopens for a new slash", () => {
  const {editor} = setup("<p>Hello</p>"), root = editor.editable, menu = menuOf(editor.node), text = root.firstChild!.firstChild as Text;
  typeInto(root, text, "Hello /"); expect(menu.hidden).toBe(false);
  expect(press(root, "Escape").defaultPrevented).toBe(true); expect(menu.hidden).toBe(true);
  typeInto(root, text, "Hello /b"); expect(menu.hidden).toBe(true); // the same slash stays dismissed
  typeInto(root, text, "Hello /zzz"); expect(menu.hidden).toBe(true);
  typeInto(root, text, "Hello b /b"); expect(menu.hidden).toBe(false);
  typeInto(root, text, "Hello b /b "); expect(menu.hidden).toBe(true);
  typeInto(root, text, "Hello b /b"); press(root, "Enter");
  expect(editor.exportHtml()).toBe("<ul><li>Hello b </li></ul>");
});
test("the slash list is not offered inside a list item and never in a readonly editor", () => {
  const {editor} = setup("<ul><li>item</li></ul>"), text = editor.editable.querySelector("li")!.firstChild as Text;
  text.data = "item /"; select(editor.editable, text, 6); editor.editable.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  expect(menuOf(editor.node).hidden).toBe(true);
  const other = setup("<p>x</p>"); other.editor.setReadonly(true); const t = other.editor.editable.firstChild!.firstChild as Text;
  t.data = "x /"; select(other.editor.editable, t, 3); other.editor.editable.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  expect(menuOf(other.editor.node).hidden).toBe(true);
});
test("undo and redo stay on the keyboard without any visible strip", () => {
  const {editor} = setup("<p>Hello</p>"), root = editor.editable;
  type(root, "Hello!"); press(root, "z", {ctrlKey: true}); expect(editor.exportHtml()).toBe("<p>Hello</p>");
  press(root, "z", {ctrlKey: true, shiftKey: true}); expect(editor.exportHtml()).toBe("Hello!");
  expect(toolbarOf(editor.node).hidden).toBe(true);
});

// Review round 1 (ecc-typescript-html-float-r1): each finding has a test here.
const rects = new Map<Range, DOMRect>();
function stubRangeGeometry(): {set: (top: number) => void} {
  let top = 300;
  const proto = Range.prototype as unknown as {getBoundingClientRect?: () => DOMRect};
  const before = proto.getBoundingClientRect;
  proto.getBoundingClientRect = () => new DOMRect(100, top, 50, 20);
  disposers.push(() => { if (before) proto.getBoundingClientRect = before; else delete proto.getBoundingClientRect; rects.clear(); });
  return {set: value => { top = value; }};
}
test("MUST 1: the toolbar comes back when the selection scrolls out of view and back", () => {
  const geometry = stubRangeGeometry(), {editor} = setup(), root = editor.editable, text = root.firstChild!.firstChild!, toolbar = toolbarOf(editor.node);
  select(root, text, 0, text, 5); expect(toolbar.hidden).toBe(false);
  geometry.set(-200); window.dispatchEvent(new Event("scroll")); expect(toolbar.hidden).toBe(true);
  geometry.set(200); window.dispatchEvent(new Event("scroll")); expect(toolbar.hidden).toBe(false);
});
test("MUST 2: the toolbar and the slash list hide when focus leaves the field", () => {
  const {editor} = setup(), root = editor.editable, text = root.firstChild!.firstChild!, toolbar = toolbarOf(editor.node), menu = menuOf(editor.node);
  const other = document.createElement("input"); document.body.append(other);
  select(root, text, 0, text, 5); expect(toolbar.hidden).toBe(false);
  other.focus(); expect(toolbar.hidden).toBe(true); // the document selection is still inside the field
  const second = setup(""); type(second.editor.editable, "/"); expect(menuOf(second.editor.node).hidden).toBe(false);
  other.focus(); expect(menuOf(second.editor.node).hidden).toBe(true); expect(menu.hidden).toBe(true);
});
test("SHOULD 3: no slash command inside a code block or inline code", () => {
  const {editor} = setup("<pre>cd </pre>"), root = editor.editable, text = root.querySelector("pre")!.firstChild as Text;
  typeInto(root, text, "cd /h"); expect(menuOf(editor.node).hidden).toBe(true);
  const inline = setup("<p><code>x</code></p>"), code = inline.editor.editable.querySelector("code")!.firstChild as Text;
  typeInto(inline.editor.editable, code, "x /"); expect(menuOf(inline.editor.node).hidden).toBe(true);
});
test("SHOULD 4: a line break is added only when the new block has nothing else", () => {
  const {editor} = setup("<p>/<strong>x</strong></p>"), root = editor.editable, text = root.firstChild!.firstChild as Text;
  select(root, text, 1); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  expect(menuOf(editor.node).hidden).toBe(false); press(root, "Enter");
  expect(editor.exportHtml()).toBe("<p><strong>x</strong></p>"); // Text keeps a paragraph, no stray <br>
  const heading = setup("<p>/<strong>x</strong></p>"), hroot = heading.editor.editable, htext = hroot.firstChild!.firstChild as Text;
  select(hroot, htext, 1); hroot.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  press(hroot, "ArrowDown"); press(hroot, "Enter"); expect(heading.editor.exportHtml()).toBe("<h1><strong>x</strong></h1>");
});
test("SHOULD 5: Enter that commits an IME composition does not apply a command", () => {
  const {editor} = setup(""), root = editor.editable; type(root, "/");
  expect(press(root, "Enter", {isComposing: true}).defaultPrevented).toBe(false);
  const legacy = press(root, "Enter", {keyCode: 229}); expect(legacy.defaultPrevented).toBe(false);
  expect(editor.exportHtml()).toBe("/");
});
test("SHOULD 6: the chosen option is announced through the status element", () => {
  const {editor} = setup(""), root = editor.editable, status = editor.node.querySelector(".html-editor-status")!;
  type(root, "/head"); expect(status.textContent).toBe("Heading 1, 1 of 3");
  press(root, "ArrowDown"); expect(status.textContent).toBe("Heading 2, 2 of 3");
});
test("SHOULD 7: Alt+F10 focuses the toolbar and the field names its shortcuts", () => {
  const {editor} = setup(), root = editor.editable, text = root.firstChild!.firstChild!;
  expect(root.getAttribute("aria-keyshortcuts")).toContain("Alt+F10");
  select(root, text, 0, text, 5); press(root, "F10", {altKey: true, code: "F10"});
  expect(document.activeElement).toBe(toolbarOf(editor.node).querySelector("button"));
});
test("SHOULD 8 and NITs: no placeholder on a readonly field; no slash trigger in the middle of a word", () => {
  const {editor} = setup(""); expect(editor.editable.hasAttribute("aria-placeholder")).toBe(true);
  editor.setReadonly(true); expect(editor.editable.hasAttribute("aria-placeholder")).toBe(false);
  const word = setup("<p><strong>and</strong>/or</p>"), root = word.editor.editable, text = root.querySelector("p")!.lastChild as Text;
  select(root, text, 1); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  expect(menuOf(word.editor.node).hidden).toBe(true);
  const start = setup("<p><strong>x</strong> /</p>"), sroot = start.editor.editable, stext = sroot.querySelector("p")!.lastChild as Text;
  select(sroot, stext, 2); sroot.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: "/"}));
  expect(menuOf(start.editor.node).hidden).toBe(false);
});
test("the toolbar is one row of icon buttons with a divider before undo and redo", () => {
  const {editor} = setup(), toolbar = toolbarOf(editor.node);
  expect([...toolbar.children].map(child => (child as HTMLElement).dataset["htmlCommand"] ?? child.getAttribute("role"))).toEqual(["bold", "italic", "underline", "strike", "separator", "undo", "redo"]);
  for (const name of ["undo", "redo"]) { const button = toolbar.querySelector<HTMLElement>(`[data-html-command="${name}"]`)!; expect(button.querySelector("svg")).not.toBeNull(); expect(button.getAttribute("aria-label")).toBeTruthy(); expect(button.title).toBeTruthy(); }
});
test("pointer-down on any toolbar button, undo and redo included, never takes the field's focus", () => {
  const {editor} = setup(), toolbar = toolbarOf(editor.node);
  for (const button of toolbar.querySelectorAll<HTMLElement>("button")) {
    for (const type of ["pointerdown", "mousedown"]) {
      const event = new Event(type, {bubbles: true, cancelable: true});
      button.dispatchEvent(event);
      expect(event.defaultPrevented, `${button.dataset["htmlCommand"]} ${type}`).toBe(true);
    }
  }
});
test("test 3: one undo after a slash command and one after a mark toggle restore the previous state", () => {
  const {editor} = setup(""), root = editor.editable; type(root, "/"); type(root, "/head"); press(root, "Enter");
  expect(editor.exportHtml()).toBe("<h1><br></h1>"); press(root, "z", {ctrlKey: true}); expect(editor.exportHtml()).toBe("");
  press(root, "z", {ctrlKey: true, shiftKey: true}); expect(editor.exportHtml()).toBe("<h1><br></h1>");
  const marked = setup(), mroot = marked.editor.editable, text = mroot.firstChild!.firstChild!;
  select(mroot, text, 0, text, 5); toolbarOf(marked.editor.node).querySelector<HTMLButtonElement>('[data-html-command="strike"]')!.click();
  expect(marked.editor.exportHtml()).toBe("<p><s>Hello</s> world</p>"); marked.editor.undo(); expect(marked.editor.exportHtml()).toBe("<p>Hello world</p>");
});
test("test 4: toggleMark across two blocks and inside a list item", () => {
  const {editor} = setup("<p>ab</p><p>cd</p><ul><li>ef</li></ul>"), root = editor.editable;
  select(root, root.children[0]!.firstChild!, 1, root.children[1]!.firstChild!, 1);
  expect(toggleMark(root, "bold")).toBe(true); expect(root.innerHTML).toBe("<p>a<strong>b</strong></p><p><strong>c</strong>d</p><ul><li>ef</li></ul>");
  const li = root.querySelector("li")!.firstChild!; select(root, li, 0, li, 1);
  expect(toggleMark(root, "italic")).toBe(true); expect(root.querySelector("li")!.innerHTML).toBe("<em>e</em>f");
});
test("test 5: two editors on one form, the second takes the selection, then one is disposed with its list open", () => {
  const first = setup(""), second = setup("<p>Second text</p>");
  type(first.editor.editable, "/"); expect(menuOf(first.editor.node).hidden).toBe(false);
  const text = second.editor.editable.firstChild!.firstChild!; select(second.editor.editable, text, 0, text, 6);
  expect(menuOf(first.editor.node).hidden).toBe(true); expect(toolbarOf(first.editor.node).hidden).toBe(true); expect(toolbarOf(second.editor.node).hidden).toBe(false);
  type(first.editor.editable, "/"); expect(menuOf(first.editor.node).hidden).toBe(false);
  first.editor.dispose(); expect(menuOf(first.editor.node).hidden).toBe(true);
  window.dispatchEvent(new Event("scroll")); window.dispatchEvent(new Event("resize")); document.dispatchEvent(new Event("selectionchange"));
  expect(toolbarOf(first.editor.node).hidden).toBe(true);
});

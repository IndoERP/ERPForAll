/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createHtmlEditor } from "../src/client/html-editor.js";
import { admitHtml, exportHtml, htmlEditorLimits, markHtmlEditorTransient, plainHtml } from "../src/client/html-editor-document.js";
import { MessageServices } from "../src/client/messages.js";
import { ClientError } from "../src/client/transport.js";

// Accepted HTML must read back identically (lead review r1, MUST 1, triggers a-e). Every accepted
// state is one the history can present again, so undo, redo and refusal recovery cannot get stuck.
const limits = htmlEditorLimits();
const disposers: (() => void)[] = [];
afterEach(() => { for (const dispose of disposers.splice(0)) dispose(); document.getSelection()?.removeAllRanges(); document.body.replaceChildren(); });
function setup(value: string) {
  const report = vi.fn(), change = vi.fn(), messages = new MessageServices(document.body, vi.fn());
  const editor = createHtmlEditor({environment: {messages, report}, label: "Notes", value, change});
  document.body.append(editor.node); disposers.push(() => editor.dispose(), () => messages.dispose());
  return {editor, report, change};
}
function select(root: HTMLElement, node: Node, offset: number): void { root.focus(); document.getSelection()!.setBaseAndExtent(node, offset, node, offset); document.dispatchEvent(new Event("selectionchange")); }
function paste(root: HTMLElement, values: Record<string, string>): void {
  const event = new Event("paste", {bubbles: true, cancelable: true});
  Object.defineProperty(event, "clipboardData", {value: {types: Object.keys(values), files: [], getData: (key: string) => values[key] ?? ""}});
  root.dispatchEvent(event);
}
function edit(root: HTMLElement, text: string): void { root.textContent = text; select(root, root.firstChild!, text.length); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText", data: text})); }
const refused = (action: () => unknown, kind: string): void => {
  let error: unknown; try { action(); } catch (value) { error = value; }
  expect(error).toBeInstanceOf(ClientError); expect((error as ClientError).kind).toBe(kind);
};

/// (partial: stability of accepted content for history; trigger a, pasted list item in a list item)
test("pasting a list item into a list item is refused and history keeps working", () => {
  const {editor, report, change} = setup("<ul><li>one</li><li>two</li></ul>"), root = editor.editable;
  select(root, root.querySelector("li")!.firstChild!, 1); paste(root, {"text/html": "<li>x</li>"});
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "unsupported", message: "HTML editor cannot place a list item inside a list item."}));
  expect(editor.exportHtml()).toBe("<ul><li>one</li><li>two</li></ul>"); expect(change).not.toHaveBeenCalled();
  select(root, root.querySelector("li")!.firstChild!, 1); paste(root, {"text/plain": "!"});
  expect(editor.exportHtml()).toBe("<ul><li>o!ne</li><li>two</li></ul>"); editor.undo(); editor.redo();
  expect(editor.exportHtml()).toBe("<ul><li>o!ne</li><li>two</li></ul>"); expect(report).toHaveBeenCalledTimes(1);
});

/// (partial: stability of accepted content for history; trigger b, carriage return)
test("a carriage return character cannot enter accepted content", () => {
  refused(() => admitHtml(document, "<p>a&#13;b</p>", limits, "initial"), "unsupported");
  const {editor, report} = setup("<p>a</p>"), root = editor.editable;
  root.firstChild!.firstChild!.nodeValue = "a\rb"; root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText"}));
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "unsupported"})); expect(editor.exportHtml()).toBe("<p>a</p>");
  edit(root, "ok"); editor.undo(); editor.redo(); expect(editor.exportHtml()).toBe("ok");
});

/// (partial: stability of accepted content for history; trigger c, pre with a leading line feed)
test("a pre whose text starts with a line feed serializes the parser-dropped line feed", () => {
  const admitted = admitHtml(document, "<pre>\n\nx</pre>", limits, "initial");
  expect(admitted.html).toBe("<pre>\n\nx</pre>"); expect(admitHtml(document, admitted.html, limits, "initial").html).toBe(admitted.html);
  expect(admitHtml(document, "<pre>\nx</pre>", limits, "initial").html).toBe("<pre>x</pre>");
  const {editor, report} = setup("<pre>\n\nx</pre>"), root = editor.editable;
  const text = root.querySelector("pre")!.firstChild as Text; expect(text.data).toBe("\nx");
  text.data = "\ny"; root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText"}));
  expect(editor.exportHtml()).toBe("<pre>\n\ny</pre>"); editor.undo(); expect(editor.exportHtml()).toBe("<pre>\n\nx</pre>"); editor.redo(); expect(editor.exportHtml()).toBe("<pre>\n\ny</pre>"); expect(report).not.toHaveBeenCalled();
  const host = document.createElement("div"), live = document.createElement("pre"); live.append("\nz"); host.append(live); expect(exportHtml(host, limits)).toBe("<pre>\n\nz</pre>");
});

/// (partial: plain paste of NUL text is refused before insertion)
test("plain text containing NUL is refused and leaves the document unchanged", () => {
  refused(() => plainHtml(document, "a\u0000b", limits), "validation");
  const {editor, report} = setup("<p>Hello</p>"), root = editor.editable;
  select(root, root.firstChild!.firstChild!, 1); paste(root, {"text/plain": "a\u0000b"});
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "validation", message: "HTML editor text must not contain NUL characters."}));
  expect(editor.exportHtml()).toBe("<p>Hello</p>"); expect(editor.state().canUndo).toBe(false);
});

/// (partial: stability of accepted content for history; trigger e, heading in heading)
test("pasting a heading into a heading is refused before moving any branch", () => {
  const {editor, report} = setup("<h1><p>a</p></h1>"), root = editor.editable;
  select(root, root.querySelector("p")!.firstChild!, 1); paste(root, {"text/html": "<h2>x</h2>"});
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "unsupported", message: "HTML editor cannot place a heading inside a heading."}));
  expect(editor.exportHtml()).toBe("<h1><p>a</p></h1>"); expect(editor.state().canUndo).toBe(false);
});

/// (partial: native input producing an unstable tree is refused and the accepted tree restored)
test("a native edit that nests a list item is refused and recovery reaches the reporter", () => {
  const {editor, report, change} = setup("<ul><li>one</li></ul>"), root = editor.editable;
  const item = root.querySelector("li")!; item.append(document.createElement("li"));
  root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertParagraph"}));
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "unsupported", message: "HTML editor content would change when it is parsed again; the edit was refused."}));
  expect(editor.exportHtml()).toBe("<ul><li>one</li></ul>"); expect(change).not.toHaveBeenCalled(); expect(root.querySelectorAll("li")).toHaveLength(1);
});

/// (partial: undo moves the history index only after the previous snapshot is presented)
test("a failing present leaves the history index unmoved", () => {
  const {editor, report} = setup("<p>a</p>"), root = editor.editable; edit(root, "b");
  const create = document.createElement.bind(document);
  const spy = vi.spyOn(document, "createElement").mockImplementation(((name: string) => { if (name === "template") throw new Error("present failed"); return create(name); }) as typeof document.createElement);
  expect(() => editor.undo()).toThrow("present failed"); spy.mockRestore();
  expect(editor.state()).toMatchObject({canUndo: true, canRedo: false}); expect(report).not.toHaveBeenCalled();
  editor.undo(); expect(editor.exportHtml()).toBe("<p>a</p>"); expect(editor.state()).toMatchObject({canUndo: false, canRedo: true});
});

/// (partial: protected content guard fails closed without a beforeinput record; protection is an internal capability)
test("an input without a recorded protected snapshot cannot delete a protected island", () => {
  const {editor, report, change} = setup("<p>Open</p><p>Locked</p>"), root = editor.editable, island = root.children[1]!;
  markHtmlEditorTransient(island); island.setAttribute("contenteditable", "false");
  select(root, root.firstChild!.firstChild!, 1); root.dispatchEvent(new InputEvent("beforeinput", {bubbles: true, cancelable: true, inputType: "insertText", data: "x"}));
  root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "insertText"})); // consumes the record
  root.children[1]!.remove(); root.dispatchEvent(new InputEvent("input", {bubbles: true, inputType: "deleteContentForward"}));
  expect(report).toHaveBeenLastCalledWith(expect.objectContaining({kind: "validation", message: "HTML editor edits cannot change protected content."}));
  expect(root.children[1]?.getAttribute("contenteditable")).toBe("false"); expect(editor.exportHtml()).toBe("<p>Open</p><p>Locked</p>"); expect(change).not.toHaveBeenCalled();
});

/// (partial: unrouted native format commands are prevented; browser proof is the Chromium driver's)
test("unrouted format input types are prevented while text input is not", () => {
  const {editor} = setup("<p>Hello</p>"), root = editor.editable; select(root, root.firstChild!.firstChild!, 1);
  for (const inputType of ["formatItalic", "formatUnderline", "formatStrikeThrough", "formatSetBlockTextDirection", "formatBold"]) {
    const event = new InputEvent("beforeinput", {bubbles: true, cancelable: true, inputType}); root.dispatchEvent(event); expect(event.defaultPrevented, inputType).toBe(true);
  }
  const text = new InputEvent("beforeinput", {bubbles: true, cancelable: true, inputType: "insertText", data: "x"}); root.dispatchEvent(text); expect(text.defaultPrevented).toBe(false);
});

/// (partial: an edit larger than inputBytes is refused, so recovery and undo can present every accepted state)
test("an edit over a lowered inputBytes limit is refused and history keeps working", () => {
  const report = vi.fn(), change = vi.fn(), messages = new MessageServices(document.body, vi.fn());
  const editor = createHtmlEditor({environment: {messages, report}, label: "Notes", value: "<p>a</p>", change, limits: {inputBytes: 64}});
  document.body.append(editor.node); disposers.push(() => editor.dispose(), () => messages.dispose());
  const root = editor.editable, long = "x".repeat(80);
  edit(root, long);
  expect(report).toHaveBeenCalledWith(expect.objectContaining({kind: "validation", message: "HTML editor input exceeds inputBytes limit."}));
  expect(report).toHaveBeenCalledTimes(1); expect(editor.exportHtml()).toBe("<p>a</p>"); expect(change).not.toHaveBeenCalled();
  edit(root, "ok"); expect(editor.exportHtml()).toBe("ok"); editor.undo(); expect(editor.exportHtml()).toBe("<p>a</p>");
  editor.redo(); expect(editor.exportHtml()).toBe("ok"); expect(report).toHaveBeenCalledTimes(1);
  // Reconstruction charges 63 bytes; the serialized form adds the <pre> line feed (64), which the limit now counts.
  refused(() => admitHtml(document, "<pre>\n\n" + "y".repeat(51) + "</pre>", htmlEditorLimits({outputBytes: 63}), "initial"), "validation");
});

/// (partial: pasted full documents drop the title text)
test("pasted document title text is dropped", () => {
  expect(admitHtml(document, "<title>Doc title</title><p>Body</p>", limits, "paste").html).toBe("<p>Body</p>");
});

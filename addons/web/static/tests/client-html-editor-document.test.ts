/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { admitHtml, exportHtml, htmlEditorLimits, plainHtml, markHtmlEditorPlaceholder, markHtmlEditorTransient } from "../src/client/html-editor-document.js";
import { ClientError } from "../src/client/transport.js";

function refuses(action: () => unknown, kind: string, message: string): void {
  try { action(); throw new Error("Expected refusal"); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind, message }); }
}
const limits = htmlEditorLimits();
/// (partial: safe document admission and canonical export)
test("document admits structural vocabulary and canonicalizes bold and italic", () => {
  const source = "<h2>Title</h2><p><b>bold</b> <i>italic</i><br><u>u</u><s>s</s></p><blockquote><pre><code>x &lt; y</code></pre></blockquote><ul><li>a</li></ul><ol><li>b</li></ol>";
  const value = admitHtml(document, source, limits, "initial");
  expect(value.html).toBe(source.replaceAll("<b>", "<strong>").replaceAll("</b>", "</strong>").replaceAll("<i>", "<em>").replaceAll("</i>", "</em>"));
  expect(exportHtml(value.fragment, limits)).toBe(value.html);
  expect(value.simplified).toBe(false);
  expect(typeof value.html).toBe("string");
});
test("empty, Unicode, CRLF, escaping and malformed supported HTML are deterministic", () => {
  expect(admitHtml(document, "", limits, "initial").html).toBe("");
  expect(admitHtml(document, "<p>é🙂\r\nx &amp; y", limits, "initial").html).toBe("<p>é🙂\nx &amp; y</p>");
  expect(exportHtml(plainHtml(document, "<img>\r\né🙂\n&", limits), limits)).toBe("&lt;img&gt;<br>é🙂<br>&amp;");
});
test("initial content refuses unsupported tags, attributes, comments and nonstrings", () => {
  for (const source of ["<img src=x>", "<p onclick='x'>a</p>", "<p style='color:red'>a</p>", "<a href='javascript:x'>a</a>", "<svg><text>x</text></svg>", "<x-widget>x</x-widget>", "<!--x-->"])
    refuses(() => admitHtml(document, source, limits, "initial"), "unsupported", "HTML editor initial content contains unsupported markup.");
  for (const source of [null, 3, {}, { html: "ok" }])
    refuses(() => admitHtml(document, source, limits, "initial"), "validation", "HTML editor value must be a string.");
});
/// (partial: paste admission strips active trees and reports simplification)
test("paste drops active trees, unwraps ordinary unsupported formatting and strips attributes", () => {
  const value = admitHtml(document, "<p class=x onclick=x>safe<a href='https://example.test'>link</a><script>bad</script><style>bad</style><iframe>bad</iframe><svg><text>bad</text></svg><math><mi>bad</mi></math><img src=x><x-thing>word</x-thing><!--bad--></p>", limits, "paste");
  expect(value.html).toBe("<p>safelinkword</p>"); expect(value.simplified).toBe(true);
  expect(admitHtml(document, "<p>safe</p>", limits, "paste").simplified).toBe(false);
});
test("source byte ceiling is checked before the inert parser and Unicode counts as UTF-8", () => {
  expect(admitHtml(document, "é", htmlEditorLimits({ inputBytes: 2 }), "initial").html).toBe("é");
  refuses(() => admitHtml(document, "é", htmlEditorLimits({ inputBytes: 1 }), "initial"), "validation", "HTML editor input exceeds inputBytes limit.");
});
test("output, node and depth admission limits accept boundaries and refuse overflow", () => {
  expect(admitHtml(document, "<b>x</b>", htmlEditorLimits({ outputBytes: 18, nodes: 2, depth: 2 }), "initial").html).toBe("<strong>x</strong>");
  refuses(() => admitHtml(document, "<b>x</b>", htmlEditorLimits({ outputBytes: 16 }), "initial"), "validation", "HTML editor output exceeds outputBytes limit.");
  refuses(() => admitHtml(document, "<p>x</p>", htmlEditorLimits({ nodes: 1 }), "initial"), "validation", "HTML editor document exceeds nodes limit.");
  refuses(() => admitHtml(document, "<p>x</p>", htmlEditorLimits({ depth: 1 }), "initial"), "validation", "HTML editor document exceeds depth limit.");
  refuses(() => admitHtml(document, "<x>".repeat(65) + "x" + "</x>".repeat(65), limits, "paste"), "validation", "HTML editor document exceeds depth limit.");
});
test("export reconstructs without mutating live content and refuses untrusted descendants", () => {
  const root = document.createElement("div"); root.setAttribute("contenteditable", "true"); root.setAttribute("aria-label", "Editor");
  const value = admitHtml(document, "<p><strong>safe</strong></p>", limits, "initial"); root.append(value.fragment);
  const before = root.cloneNode(true); expect(exportHtml(root, limits)).toBe(value.html); expect(root.isEqualNode(before)).toBe(true);
  const image = document.createElement("img"); image.src = "https://example.test/payload"; root.append(image);
  refuses(() => exportHtml(root, limits), "unsupported", "HTML editor export contains unsupported markup.");
});
test("limit overrides are closed positive safe integers bounded by defaults", () => {
  expect(Object.isFrozen(limits)).toBe(true);
  for (const value of [0, -1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER, undefined])
    refuses(() => htmlEditorLimits({ nodes: value }), "validation", "HTML editor limit nodes must be a positive safe integer no greater than 10000.");
  refuses(() => htmlEditorLimits({ surprise: 1 } as never), "validation", "Unknown HTML editor limit: surprise.");
  refuses(() => htmlEditorLimits(null as never), "validation", "HTML editor limits must be an object.");
});

test("export only trusts ownership held outside document attributes and removes empty scaffolding", () => {
  const root = document.createElement("div");
  const p = document.createElement("p"); p.append("safe"); p.setAttribute("contenteditable", "false");
  root.append(p); markHtmlEditorTransient(p);
  const placeholder = document.createElement("div"); placeholder.append("placeholder"); root.append(placeholder); markHtmlEditorPlaceholder(placeholder);
  expect(exportHtml(root, limits)).toBe("<p>safe</p>"); expect(p.getAttribute("contenteditable")).toBe("false"); expect(placeholder.isConnected).toBe(false);
  const forged = document.createElement("p"); forged.setAttribute("data-html-editor-placeholder", "true"); forged.append("kept"); root.append(forged);
  refuses(() => exportHtml(root, limits), "unsupported", "HTML editor export contains unsupported markup.");
  expect(admitHtml(document, "<p><br></p>", limits, "initial").html).toBe("");
});

test("native parser omissions cannot silently admit unsupported root wrappers or declarations", () => {
  for (const source of ["<!DOCTYPE html><p>x</p>", "<html><body><p>x</p></body></html>", "<head><title>x</title></head><p>y</p>", "<p>x</p></unsupported>", "<p onclick=x", "<x".repeat(20)])
    refuses(() => admitHtml(document, source, limits, "initial"), "unsupported", "HTML editor initial content contains unsupported markup.");
  expect(admitHtml(document, "<!DOCTYPE html><p>x</p>", limits, "paste")).toMatchObject({ html: "<p>x</p>", simplified: true });
});
test("oversized source refuses before parsing and oversized live text refuses export", () => {
  const create = vi.spyOn(document, "createElement");
  try {
    refuses(() => admitHtml(document, "xx", htmlEditorLimits({ inputBytes: 1 }), "initial"), "validation", "HTML editor input exceeds inputBytes limit.");
    expect(create).not.toHaveBeenCalled();
  } finally { create.mockRestore(); }
  const root = document.createElement("div"); root.append("<".repeat(30));
  refuses(() => exportHtml(root, htmlEditorLimits({ outputBytes: 3 })), "validation", "HTML editor output exceeds outputBytes limit.");
});
test("newline-only scaffolding is empty while internal and trailing text line breaks survive", () => {
  expect(exportHtml(plainHtml(document, "\n\n", limits), limits)).toBe("");
  expect(exportHtml(plainHtml(document, "a\n\n", limits), limits)).toBe("a<br><br>");
});

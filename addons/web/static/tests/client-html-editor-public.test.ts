/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, expectTypeOf, test } from "vitest";
import * as api from "../src/client/public.js";
import type { HtmlEditorOptions, HtmlEditorHandle, HtmlEditorLimits, EditorEnvironment } from "../src/client/public.js";
import { MessageServices } from "../src/client/messages.js";

function richNotes(environment: EditorEnvironment, changed: (html: string) => void): HtmlEditorHandle {
  return api.createHtmlEditor({environment, label: "Notes", value: "<p>Draft notes</p>", change: changed, clipboardMode: "rich"});
}

test("trusted addon entry exposes ordinary string HTML authoring without storage authority", () => {
  expect(typeof api.createHtmlEditor).toBe("function");
  expectTypeOf<ReturnType<typeof api.createHtmlEditor>>().toEqualTypeOf<HtmlEditorHandle>();
  expectTypeOf<ReturnType<HtmlEditorHandle["exportHtml"]>>().toEqualTypeOf<string>();
  expectTypeOf<HtmlEditorOptions["change"]>().toEqualTypeOf<(html:string)=>void|Promise<void>>();
  expectTypeOf<HtmlEditorOptions["limits"]>().toEqualTypeOf<Partial<HtmlEditorLimits>|undefined>();
  const messages = new MessageServices(document.body, error => { throw error; });
  const notes = richNotes({messages, report: error => { throw error; }}, () => {});
  expect(notes.exportHtml()).toBe("<p>Draft notes</p>"); notes.dispose(); messages.dispose();
});

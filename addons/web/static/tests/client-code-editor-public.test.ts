/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { createCodeEditor } from "../src/client/public.js";
import type { CodeEditorHandle, CodeEditorOptions, CodeEditorChange } from "../src/client/public.js";

// The addon-guide example consumes only the trusted public entry and owner lifetime.
function sourceEditor(host: HTMLElement, signal: AbortSignal, change: (value: CodeEditorChange) => void, report: (error: unknown) => void): CodeEditorHandle {
  const options: CodeEditorOptions = { label: "Template source", sessionId: "template", value: "<section/>", mode: "xml", signal, onChange: change, report };
  const editor = createCodeEditor(options); host.append(editor.node); return editor;
}

test("trusted addon code editor exposes source operations without selecting persistence or leaking its engine", () => {
  const host = document.createElement("div"); document.body.append(host); const owner = new AbortController(), change = vi.fn(), report = vi.fn(), fetcher = vi.spyOn(globalThis, "fetch");
  const editor = sourceEditor(host, owner.signal, change, report);
  try {
    editor.setValue("<section>Owner value</section>"); expect(editor.getValue()).toBe("<section>Owner value</section>"); expect(change).not.toHaveBeenCalled();
    editor.setSession({ sessionId: "script", value: "return 1", mode: "javascript" }); expect(editor.getSessionId()).toBe("script");
    editor.setSession({ sessionId: "template", value: "<section>Owner value</section>", mode: "xml" }); editor.releaseSession("script");
    expect(Object.keys(editor)).not.toContain("engine"); expect(Object.keys(editor)).not.toContain("ace"); expect(fetcher).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled();
    owner.abort(); expect(editor.history.canUndo).toBe(false); expect(editor.history.canRedo).toBe(false);
  } finally { editor.dispose(); host.remove(); fetcher.mockRestore(); }
});

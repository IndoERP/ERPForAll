/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import ace from "ace-builds";
import { markup } from "@erpforall/elang";
import { createCodeEditor } from "../src/client/code-editor.js";
import type { CodeEditorHandle, CodeEditorMode, CodeEditorOptions } from "../src/client/code-editor-types.js";
import { ClientError } from "../src/client/transport.js";

const controls: CodeEditorHandle[] = [];
afterEach(() => { for (const control of controls.splice(0)) control.dispose(); document.body.replaceChildren(); vi.restoreAllMocks(); });
function fixture(extra: Partial<CodeEditorOptions> = {}) {
  const onChange = vi.fn(), report = vi.fn(), onBlur = vi.fn();
  const control = createCodeEditor({ label: "Source", sessionId: "one", value: "first\nsecond", onChange, report, onBlur, ...extra });
  controls.push(control); document.body.append(control.node);
  const surface = control.node.querySelector<HTMLElement>(".ace_editor");
  if (!surface) throw new Error("The code editor did not mount its real engine.");
  return { control, engine: ace.edit(surface), onChange, report, onBlur };
}
function expectFailure(run: () => unknown, message: string): void {
  try { run(); throw new Error("Expected code editor admission to fail."); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message }); }
}
function position(engine: ReturnType<typeof ace.edit>, row: number, column: number): void {
  // Ace selection.moveCursorTo moves only its lead. Explicitly collapse selection for
  // these insertion/cursor-only scenarios; selected-text replacement is tested separately.
  engine.selection.moveCursorTo(row, column); engine.selection.clearSelection();
}
async function userInsert(engine: ReturnType<typeof ace.edit>, text: string): Promise<void> {
  engine.insert(text); await Promise.resolve();
}
const modes: readonly CodeEditorMode[] = ["text", "javascript", "xml", "qweb", "scss", "python", "json", "bash"];

test("code values, admitted modes and independent sessions retain owner and cursor boundaries", async () => {
  const { control, engine, onChange, report } = fixture();
  expect(control.getValue()).toBe("first\nsecond"); expect(control.getSessionId()).toBe("one");
  expect(onChange).not.toHaveBeenCalled();
  position(engine, 1, 3); await userInsert(engine, "!");
  expect(control.getValue()).toBe("first\nsec!ond");
  expect(onChange).toHaveBeenCalledExactlyOnceWith({ sessionId: "one", value: "first\nsec!ond" });
  await vi.waitFor(() => expect(control.history.canUndo).toBe(true));
  const editedCursor = engine.getCursorPosition();
  control.setValue(control.getValue());
  expect(onChange).toHaveBeenCalledTimes(1); expect(control.history.canUndo).toBe(true); expect(engine.getCursorPosition()).toEqual(editedCursor);
  // Cursor movement without a text change must be captured when leaving a session.
  position(engine, 0, 2);
  control.setSession({ sessionId: 1, value: "numeric", mode: "python" });
  expect(control.getSessionId()).toBe(1); expect(control.history.canUndo).toBe(false);
  position(engine, 0, 4);
  control.setSession({ sessionId: "1", value: "string", mode: "json" });
  expect(control.getValue()).toBe("string");
  control.setSession({ sessionId: 1, value: "numeric", mode: "python" });
  expect(engine.getCursorPosition()).toEqual({ row: 0, column: 4 });
  control.setSession({ sessionId: "one", value: "first\nsec!ond", mode: "xml" });
  expect(engine.getCursorPosition()).toEqual({ row: 0, column: 2 }); expect(control.history.canUndo).toBe(true);
  for (const mode of modes) {
    control.setSession({ sessionId: "one", value: control.getValue(), mode });
    const activeMode: unknown = Reflect.get(engine.session.getMode(), "$id");
    expect(activeMode).toBe(`ace/mode/${mode === "bash" ? "sh" : mode}`);
  }
  control.setValue("<script>literal source</script>");
  expect(control.getValue()).toBe("<script>literal source</script>"); expect(control.node.querySelector("script")).toBeNull();
  expect(control.history.canUndo).toBe(false); expect(onChange).toHaveBeenCalledTimes(1);
  expectFailure(() => control.setValue(markup("<b>trusted markup</b>") as unknown as string), "Code editor value must be primitive text.");
  expectFailure(() => control.setValue(new String("boxed") as unknown as string), "Code editor value must be primitive text.");
  expect(control.getValue()).toBe("<script>literal source</script>"); expect(report).not.toHaveBeenCalled();
});

test("code readonly and explicit history follow the active session and disconnect at disposal", async () => {
  const { control, engine, onChange } = fixture({ value: "initial" });
  const changes = vi.fn(), unsubscribe = control.history.subscribe(changes);
  expect(control.history.canUndo).toBe(false); expect(control.history.canRedo).toBe(false); expect(control.history.undo()).toBe(false);
  position(engine, 0, 7); await userInsert(engine, "!");
  await vi.waitFor(() => expect(control.history.canUndo).toBe(true));
  expect(control.history.undo()).toBe(true); expect(control.getValue()).toBe("initial"); expect(control.history.canRedo).toBe(true);
  expect(control.history.redo()).toBe(true); expect(control.getValue()).toBe("initial!"); expect(control.history.canRedo).toBe(false);
  expect(control.history.undo()).toBe(true); await userInsert(engine, "?");
  expect(control.history.canRedo).toBe(false); expect(control.history.redo()).toBe(false);
  const original = control.getValue();
  control.setSession({ sessionId: "other", value: "other" }); expect(control.history.canUndo).toBe(false);
  control.setSession({ sessionId: "one", value: original }); expect(control.history.canUndo).toBe(true);
  control.setReadonly(true);
  expect(engine.getReadOnly()).toBe(true); expect(control.node.classList.contains("code-editor-readonly")).toBe(true);
  expect(engine.getOption("highlightActiveLine")).toBe(false); expect(engine.getOption("highlightGutterLine")).toBe(false);
  expect(engine.getOption("displayIndentGuides")).toBe(false); expect(control.node.querySelector<HTMLElement>(".ace_cursor-layer")!.style.display).toBe("none");
  expectFailure(() => control.history.undo(), "Code editor is readonly.");
  engine.execCommand("insertstring", "blocked"); expect(control.getValue()).toBe(original);
  control.setReadonly(false); expect(control.history.canUndo).toBe(true);
  expect(changes).toHaveBeenCalled(); unsubscribe();
  const notified = onChange.mock.calls.length; control.dispose(); control.dispose();
  expect(control.history.canUndo).toBe(false); expect(control.history.canRedo).toBe(false);
  expectFailure(() => control.history.undo(), "Code editor is disposed.");
  expectFailure(() => control.history.redo(), "Code editor is disposed.");
  expect(onChange).toHaveBeenCalledTimes(notified);
});

test("code component labels its native input, history toolbar and inline invalid state", () => {
  const { control, report } = fixture({ label: "Template source", required: true, invalid: "Source needs correction." });
  const input = control.node.querySelector<HTMLTextAreaElement>("textarea")!, label = control.node.querySelector<HTMLLabelElement>("label");
  expect(label).not.toBeNull(); expect(label!.control).toBe(input); expect(input.labels).toContain(label);
  expect(input.getAttribute("aria-label")).toBe("Template source"); expect(input.getAttribute("aria-required")).toBe("true");
  expect(input.getAttribute("aria-invalid")).toBe("true");
  const errorId = input.getAttribute("aria-describedby")!.split(" ").find(id => document.getElementById(id)?.textContent === "Source needs correction.");
  expect(errorId).toBeDefined(); expect(document.getElementById(errorId!)?.getAttribute("role")).toBe("status");
  const buttons = [...control.node.querySelectorAll<HTMLButtonElement>("button")];
  expect(buttons.map(button => button.textContent)).toEqual(["Undo", "Redo"]); expect(buttons.every(button => button.type === "button" && button.disabled)).toBe(true);
  control.setPresentation({ invalid: "", required: false }); expect(input.hasAttribute("aria-invalid")).toBe(false); expect(input.getAttribute("aria-required")).toBe("false");
  expect(report).not.toHaveBeenCalled();
});

test("code toolbar catches unexpected owner failures and keeps inline admission local", async () => {
  const failure = new Error("Owner rejected the draft."); const onChange = vi.fn(() => { throw failure; });
  const { control, engine, report } = fixture({ onChange }); await userInsert(engine, "!");
  expect(report).toHaveBeenCalledExactlyOnceWith(failure);
  expect(control.node.querySelector(".code-editor-error")?.textContent).not.toContain("Owner rejected");
});

test("code disabled and aborted controls release keyboard entry and reject mutations", () => {
  const signal = new AbortController(); const { control } = fixture({ disabled: true, signal: signal.signal });
  expect(control.node.classList.contains("code-editor-disabled")).toBe(true); expect(control.node.querySelector<HTMLTextAreaElement>("textarea")!.tabIndex).toBe(-1);
  expectFailure(() => control.history.undo(), "Code editor is disabled.");
  control.setDisabled(false); signal.abort();
  expect(control.history.canUndo).toBe(false); expectFailure(() => control.setValue("late"), "Code editor is disposed.");
});

test("code owner callbacks may switch sessions or dispose without stale component updates", async () => {
  let control: CodeEditorHandle | undefined;
  const change = vi.fn(() => { control!.setSession({ sessionId: "replacement", value: "owner replacement", mode: "json" }); });
  const fixtureOne = fixture({ onChange: change }); control = fixtureOne.control;
  await userInsert(fixtureOne.engine, "!"); expect(control.getSessionId()).toBe("replacement"); expect(control.getValue()).toBe("owner replacement");
  expect(control.history.canUndo).toBe(false); expect(fixtureOne.report).not.toHaveBeenCalled();
  let second: CodeEditorHandle | undefined;
  const fixtureTwo = fixture({ onChange: () => { second!.dispose(); } }); second = fixtureTwo.control;
  await userInsert(fixtureTwo.engine, "!"); expect(fixtureTwo.control.history.canUndo).toBe(false); expect(fixtureTwo.report).not.toHaveBeenCalled();
});

test("code history buttons execute distinct undo and redo commands with live availability", async () => {
  const { control, engine, report } = fixture({ value: "start" }); position(engine, 0, 5);
  await userInsert(engine, "!");
  const [undo, redo] = [...control.node.querySelectorAll<HTMLButtonElement>("button")];
  await vi.waitFor(() => expect(undo!.disabled).toBe(false)); expect(redo!.disabled).toBe(true);
  undo!.click(); expect(control.getValue()).toBe("start"); expect(redo!.disabled).toBe(false);
  redo!.click(); expect(control.getValue()).toBe("start!"); expect(redo!.disabled).toBe(true); expect(report).not.toHaveBeenCalled();
});

test("code creation with an already aborted owner returns a disconnected handle without a server error", () => {
  const owner = new AbortController(); owner.abort(); const change = vi.fn(), report = vi.fn();
  const control = createCodeEditor({ label: "Closed owner", value: "retained", sessionId: "closed", signal: owner.signal, onChange: change, report });
  controls.push(control); document.body.append(control.node);
  expect(control.history.canUndo).toBe(false); expect(control.history.canRedo).toBe(false);
  expectFailure(() => control.setValue("late"), "Code editor is disposed.");
  expectFailure(() => control.history.undo(), "Code editor is disposed.");
  expect([...control.node.querySelectorAll<HTMLButtonElement>("button")].every(button => button.disabled)).toBe(true);
  expect(change).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled();
});


test("code editing replaces explicitly selected source text", async () => {
  const { control, engine, onChange } = fixture();
  engine.selection.setSelectionAnchor(0, 0); engine.selection.selectTo(0, 5);
  expect(engine.selection.isEmpty()).toBe(false);
  await userInsert(engine, "replacement");
  expect(control.getValue()).toBe("replacement\nsecond");
  expect(onChange).toHaveBeenCalledExactlyOnceWith({ sessionId: "one", value: "replacement\nsecond" });
});

test("code public readonly setter follows a reentrant history subscriber's final policy", () => {
  const { control, engine, report } = fixture(); let once = true;
  const unsubscribe = control.history.subscribe(() => { if (once) { once = false; control.setReadonly(false); } });
  control.setReadonly(true); unsubscribe();
  expect(engine.getReadOnly()).toBe(false);
  expect(control.node.querySelector<HTMLElement>(".code-editor-surface")!.dataset["codeReadonly"]).toBe("false");
  expect(control.node.classList.contains("code-editor-readonly")).toBe(false);
  expect(control.node.querySelector("textarea")!.getAttribute("aria-readonly")).toBe("false");
  expect(control.node.querySelector(".code-editor-hint")!.textContent).toBe("Press Enter to edit. Press Escape, then Tab to leave the editor.");
  expect(report).not.toHaveBeenCalled();
});

test("code public disabled setter follows a reentrant history subscriber's final policy", () => {
  const { control, engine, report } = fixture(); let once = true;
  const unsubscribe = control.history.subscribe(() => { if (once) { once = false; control.setDisabled(false); } });
  control.setDisabled(true); unsubscribe();
  expect(engine.getReadOnly()).toBe(false);
  expect(control.node.querySelector<HTMLElement>(".code-editor-surface")!.dataset["codeDisabled"]).toBe("false");
  expect(control.node.classList.contains("code-editor-disabled")).toBe(false); expect(control.node.getAttribute("aria-disabled")).toBe("false");
  expect(control.node.querySelector<HTMLTextAreaElement>("textarea")!.disabled).toBe(false);
  expect(control.node.querySelector(".code-editor-hint")!.textContent).toBe("Press Enter to edit. Press Escape, then Tab to leave the editor.");
  expect(report).not.toHaveBeenCalled();
});

test("code public presentation follows a reentrant subscriber's required validation and theme", () => {
  const { control, engine, report } = fixture(); let once = true;
  const unsubscribe = control.history.subscribe(() => {
    if (once) { once = false; control.setPresentation({ required: false, invalid: "", theme: "light", lineWrapping: false, showLineNumbers: false, maxLines: 6 }); }
  });
  control.setPresentation({ required: true, invalid: "Stale outer error.", theme: "dark", lineWrapping: true, showLineNumbers: true, maxLines: 12 }); unsubscribe();
  const input = control.node.querySelector<HTMLTextAreaElement>("textarea")!;
  expect(control.node.querySelector<HTMLElement>(".code-editor-surface")!.dataset["codeTheme"]).toBe("light");
  expect(engine.session.getUseWrapMode()).toBe(false); expect(engine.getOption("showGutter")).toBe(false); expect(engine.getOption("maxLines")).toBe(6);
  expect(control.node.dataset["codeTheme"]).toBe("light"); expect(control.node.classList.contains("code-editor-required-value")).toBe(false);
  expect(input.getAttribute("aria-required")).toBe("false"); expect(input.hasAttribute("aria-invalid")).toBe(false);
  expect(control.node.querySelector<HTMLElement>(".code-editor-required")!.hidden).toBe(true);
  expect(control.node.querySelector<HTMLElement>(".code-editor-error")!.hidden).toBe(true); expect(control.node.querySelector(".code-editor-error")!.textContent).toBe("");
  expect(report).not.toHaveBeenCalled();
});

test("code wrapper performs no late presentation commit after a history subscriber disposes it", () => {
  const { control, report } = fixture(); let once = true, atDisposal = "";
  control.history.subscribe(() => {
    if (once) { once = false; control.dispose(); atDisposal = control.node.outerHTML; }
  });
  control.setPresentation({ required: true, invalid: "Outer pending error.", theme: "dark" });
  expect(control.node.outerHTML).toBe(atDisposal); expect(control.node.classList.contains("code-editor-disposed")).toBe(true);
  expect(control.node.querySelector(".ace_editor")).toBeNull(); expect(control.history.canUndo).toBe(false); expect(control.history.canRedo).toBe(false);
  expect(report).not.toHaveBeenCalled();
});

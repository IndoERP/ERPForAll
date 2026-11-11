/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import ace from "ace-builds";
import { createCodeEngine } from "../src/client/code-editor-engine.js";
import { createCodeEditor } from "../src/client/code-editor.js";
import { CodeSessions } from "../src/client/code-editor-session.js";
import { ClientError } from "../src/client/transport.js";
import type { CodeEditorOptions } from "../src/client/code-editor-types.js";

const cleanup: (() => void)[] = [];
afterEach(() => { cleanup.splice(0).forEach(dispose => dispose()); document.body.replaceChildren(); });
function setup(patch: Partial<CodeEditorOptions> = {}) {
  const host = document.createElement("div"); document.body.append(host);
  const onChange = vi.fn(), report = vi.fn(), callbacks = { state: vi.fn(), invalid: vi.fn(), status: vi.fn() };
  const engine = createCodeEngine(host, { label: "Source", value: "original", sessionId: "a", onChange, report, ...patch }, callbacks);
  cleanup.push(engine.dispose); return { host, engine, onChange, report, callbacks };
}

function constructionFailure(inject: (native: ace.Ace.Editor) => void, beforeEdit?: () => never) {
  const host = document.createElement("div"); document.body.append(host);
  const onChange = vi.fn(), report = vi.fn(), state = vi.fn(), invalid = vi.fn(), status = vi.fn();
  const sessionDestroy = vi.spyOn(ace.EditSession.prototype, "destroy"), logicalDispose = vi.spyOn(CodeSessions.prototype, "dispose");
  const allocated = new Set<ace.Ace.EditSession>();
  let native: ace.Ace.Editor | undefined, destroy: ReturnType<typeof vi.spyOn> | undefined;
  const originalEdit = ace.edit;
  const edit = vi.spyOn(ace, "edit").mockImplementationOnce((element, options) => {
    if (beforeEdit) beforeEdit();
    native = originalEdit(element, options); allocated.add(native.session);
    destroy = vi.spyOn(native, "destroy");
    const setSession = native.setSession.bind(native);
    vi.spyOn(native, "setSession").mockImplementation(session => { if (session) allocated.add(session); setSession(session); });
    inject(native);
    return native;
  });
  let thrown: unknown;
  try {
    const engine = createCodeEngine(host, { label: "Source", value: "original", sessionId: "a", onChange, report }, { state, invalid, status });
    cleanup.push(engine.dispose);
  } catch (error) { thrown = error; }
  return {
    host, native, thrown, onChange, report, state, invalid, status,
    assertDisposed() {
      expect(host.querySelector(".code-editor-engine")).toBeNull();
      if (native) expect(destroy).toHaveBeenCalledTimes(1);
      expect(logicalDispose).toHaveBeenCalledTimes(1);
      for (const session of allocated) expect(sessionDestroy.mock.contexts.filter(context => context === session)).toHaveLength(1);
    },
    restore() {
      // The broken baseline leaks. Clean its exact test-owned objects after checking the leak.
      if (native && !destroy?.mock.calls.length) native.destroy();
      for (const session of allocated) if (!sessionDestroy.mock.contexts.includes(session)) session.destroy();
      edit.mockRestore(); vi.restoreAllMocks(); vi.unstubAllGlobals();
    },
  };
}

test("construction failure before Ace returns removes the surface and preserves its typed error", () => {
  const failure = new ClientError("server", "Injected Ace construction failure.");
  const attempt = constructionFailure(() => {}, () => { throw failure; });
  try {
    expect(attempt.thrown).toBe(failure);
    expect(attempt.thrown).toEqual(expect.objectContaining({ kind: "server", message: "Injected Ace construction failure." }));
    attempt.assertDisposed();
  } finally { attempt.restore(); }
  expect(setup().engine.getValue()).toBe("original");
});

test("construction failure on a missing command destroys the editor and original session once", () => {
  const attempt = constructionFailure(native => { delete native.commands.commands["find"]; });
  try {
    expect(attempt.thrown).toEqual(expect.objectContaining({ kind: "server", message: "The pinned code editor command is missing: find." }));
    attempt.assertDisposed(); expect(attempt.onChange).not.toHaveBeenCalled(); expect(attempt.report).not.toHaveBeenCalled();
  } finally { attempt.restore(); }
  expect(setup().engine.getValue()).toBe("original");
});

test("construction failure on missing rendering layers destroys every allocated session once", () => {
  const attempt = constructionFailure(native => { native.container.querySelector(".ace_cursor-layer")?.remove(); });
  try {
    expect(attempt.thrown).toEqual(expect.objectContaining({ kind: "server", message: "The pinned code editor rendering layers are missing." }));
    attempt.assertDisposed(); expect(attempt.state).not.toHaveBeenCalled();
  } finally { attempt.restore(); }
  expect(setup().engine.getValue()).toBe("original");
});

test("late construction failure disconnects observation listeners and pending native edits", async () => {
  const failure = new ClientError("server", "Injected observer admission failure.");
  let native: ace.Ace.Editor | undefined;
  let owningObserver: { disconnect: ReturnType<typeof vi.fn> } | undefined;
  vi.stubGlobal("ResizeObserver", class {
    observe(target: Element) {
      // Ace observes its own font/renderer nodes during construction. Fail only
      // our adapter's later observation of the owning host, after Ace returned.
      if (native && target === native.container.parentElement) { owningObserver = this; native.insert("pending"); throw failure; }
    }
    disconnect = vi.fn();
  });
  const attempt = constructionFailure(editor => { native = editor; });
  try {
    expect(attempt.thrown).toBe(failure);
    expect(attempt.thrown).toEqual(expect.objectContaining({ kind: "server", message: "Injected observer admission failure." }));
    attempt.assertDisposed(); expect(owningObserver).toBeDefined(); expect(owningObserver?.disconnect).toHaveBeenCalledTimes(1);
    await Promise.resolve();
    expect(attempt.onChange).not.toHaveBeenCalled(); expect(attempt.report).not.toHaveBeenCalled(); expect(attempt.state).not.toHaveBeenCalled();
    const paste = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(paste, "clipboardData", { value: { getData: () => "late" } });
    attempt.host.dispatchEvent(paste);
    expect(paste.defaultPrevented).toBe(false); expect(attempt.invalid).not.toHaveBeenCalled();
  } finally { attempt.restore(); }
  expect(setup().engine.getValue()).toBe("original");
});

test("code engine applies parent values without user callbacks and keeps plain HTML source inert", () => {
  const { engine, host, onChange } = setup();
  engine.setValue("<script>window.codeEditorExecuted=true</script>");
  expect(engine.getValue()).toBe("<script>window.codeEditorExecuted=true</script>"); expect(onChange).not.toHaveBeenCalled();
  expect(host.querySelector("script")).toBeNull(); expect(Reflect.get(window, "codeEditorExecuted")).toBeUndefined();
  expect(engine.history.canUndo).toBe(false); expect(engine.history.undo()).toBe(false);
});

test("code engine admission is atomic and every configured mode is a local presentation", () => {
  const { engine, host } = setup({ sessionId: "text" });
  for (const mode of ["text", "javascript", "xml", "qweb", "scss", "python", "json", "bash"] as const) {
    engine.setSession({ sessionId: mode, value: mode === "qweb" ? '<t t-out="title"/>' : "value", mode });
    expect(host.dataset["codeMode"]).toBe(mode);
  }
  expect(() => engine.setSession({ sessionId: "unsupported", value: "", mode: "unknown" } as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor mode is not supported." }));
  expect(engine.getSessionId()).toBe("bash");
});

test("readonly and disabled history commands refuse explicitly and disposal disconnects state", () => {
  const { engine, host, onChange } = setup();
  engine.setReadonly(true); expect(host.dataset["codeReadonly"]).toBe("true"); expect(engine.history.canUndo).toBe(false);
  expect(() => engine.history.undo()).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor is readonly." }));
  engine.setReadonly(false); engine.setDisabled(true);
  expect(() => engine.history.redo()).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor is disabled." }));
  engine.setDisabled(false); const state = vi.fn(); engine.history.subscribe(state);
  engine.dispose(); engine.dispose(); expect(engine.history.canUndo).toBe(false); expect(engine.history.canRedo).toBe(false);
  expect(host.querySelector(".ace_editor")).toBeNull();
  expect(() => engine.setValue("late")).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor is disposed." })); expect(onChange).not.toHaveBeenCalled();
});

test("bundled language modes tokenize source and original QWeb distinguishes directive names", () => {
  const { engine, host } = setup();
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  const samples = { javascript: "const answer = 1;", xml: '<section id="one"/>', qweb: '<t t-out="title"/>', scss: "$accent: #fff;", python: "def example():", json: '{"answer":1}', bash: 'echo "$HOME"' } as const;
  for (const [mode, value] of Object.entries(samples)) {
    engine.setSession({ sessionId: "a", value, mode: mode as keyof typeof samples });
    const tokens = native.session.getTokens(0);
    expect(tokens.map(token => token.value).join("")).toBe(value);
    expect(tokens.some(token => token.type !== "text")).toBe(true);
    if (mode === "qweb") expect(tokens).toContainEqual({ type: "keyword.control.qweb", value: "t-out" });
  }
});

test("bounded history is the sole recorder and subscribers cannot change the originating callback", async () => {
  const { engine, host, onChange, report } = setup();
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  engine.history.subscribe(() => { if (engine.getSessionId() === "a" && engine.history.canUndo) engine.setSession({ sessionId: "b", value: "parent" }); });
  native.insert("!"); await Promise.resolve();
  expect(onChange).toHaveBeenCalledExactlyOnceWith({ sessionId: "a", value: "!original" });
  expect(engine.getSessionId()).toBe("b"); expect(engine.getValue()).toBe("parent"); expect(report).not.toHaveBeenCalled();
  expect(Boolean(native.session.getUndoManager().hasUndo())).toBe(false);
  const second = setup(); const nativeSecond = ace.edit(second.host.querySelector<HTMLElement>(".ace_editor")!);
  second.engine.history.subscribe(() => { if (second.engine.history.canUndo) second.engine.dispose(); });
  nativeSecond.insert("!"); await Promise.resolve(); expect(second.onChange).not.toHaveBeenCalled(); expect(second.report).not.toHaveBeenCalled();
});

test("null optional values are refused rather than replaced with absent-value defaults", () => {
  for (const [options, message] of [[{ mode: null }, "Code editor mode is not supported."], [{ theme: null }, "Code editor theme is not supported."], [{ lineWrapping: null }, "Code editor lineWrapping must be a boolean."], [{ maxLines: null }, "Code editor maxLines must be an integer from 1 to 200."], [{ invalid: null }, "Code editor value must be primitive text."]] as const) {
    const host = document.createElement("div");
    expect(() => createCodeEngine(host, { label: "Source", value: "", sessionId: 0, onChange: vi.fn(), report: vi.fn(), ...options } as never, { state() {}, invalid() {}, status() {} })).toThrowError(expect.objectContaining({ kind: "validation", message }));
    expect(host.querySelector(".ace_editor")).toBeNull();
  }
});

test("paste and beforeinput preflight refuse oversized insertion before the engine builds source", () => {
  const { engine, host, onChange, callbacks } = setup({ value: "abc", limits: { maxDocumentBytes: 5 } });
  const input = host.querySelector<HTMLTextAreaElement>("textarea")!;
  const paste = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(paste, "clipboardData", { value: { getData: (type: string) => type === "text/plain" ? "12345" : "" } });
  input.dispatchEvent(paste);
  expect(paste.defaultPrevented).toBe(true); expect(engine.getValue()).toBe("abc"); expect(onChange).not.toHaveBeenCalled();
  expect(callbacks.invalid).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Code editor value must be at most 5 UTF-8 bytes." }));
  const before = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "éé" }); input.dispatchEvent(before);
  expect(before.defaultPrevented).toBe(true); expect(engine.getValue()).toBe("abc");
});

async function staleHistoryCommand(command: "undo" | "redo"): Promise<void> {
  let armed = false;
  const change = vi.fn(() => { if (armed) { armed = false; engine.setSession({ sessionId: "b", value: command === "undo" ? "1b" : "b" }); } });
  const { engine, host, report } = setup({ value: "a", onChange: change });
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  engine.setSession({ sessionId: "b", value: "b" }); native.insert("1"); await Promise.resolve();
  if (command === "redo") engine.history.undo();
  const expected = command === "undo" ? "1b" : "b";
  engine.setSession({ sessionId: "a", value: "a" }); armed = true; native.insert("!");
  const result = engine.history[command]();
  expect(result).toBe(false); expect(engine.getSessionId()).toBe("b"); expect(engine.getValue()).toBe(expected);
  expect(engine.history.canUndo).toBe(command === "undo"); expect(engine.history.canRedo).toBe(command === "redo");
  expect(report).not.toHaveBeenCalled();
}

test("pending edit callback switching sessions cancels the originating undo command", async () => { await staleHistoryCommand("undo"); });
test("pending edit callback switching sessions cancels the originating redo command", async () => { await staleHistoryCommand("redo"); });

test("pending edit callback replacing the same-session baseline cancels stale history mutation", () => {
  let armed = false;
  const change = vi.fn(() => {
    if (armed) { armed = false; engine.setValue("server"); native.moveCursorTo(0, 0); native.clearSelection(); native.insert("owner"); engine.getValue(); }
  });
  const { engine, host } = setup({ value: "a", onChange: change });
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  armed = true; native.insert("!");
  expect(engine.history.undo()).toBe(false); expect(engine.getValue()).toBe("ownerserver"); expect(engine.history.canUndo).toBe(true);
});

test("reentrant identical parent echoes preserve the originating pending undo", () => {
  const { engine, host, report } = setup({ value: "a", onChange: change => { engine.setValue(change.value); } });
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  native.insert("!"); expect(engine.history.undo()).toBe(true); expect(engine.getValue()).toBe("a");
  expect(engine.history.canRedo).toBe(true); expect(report).not.toHaveBeenCalled();
});

test("pending edit callbacks supersede stale owner replacements and presentation updates", async () => {
  for (const operation of ["value", "session", "readonly", "disabled", "presentation", "release"] as const) {
    let armed = false;
    const change = vi.fn(() => {
      if (armed) {
        armed = false; engine.setSession({ sessionId: "b", value: "b" }); engine.setPresentation({ maxLines: 13 });
        if (operation === "release") engine.releaseSession("a");
      }
    });
    const { engine, host, report } = setup({ value: "a", onChange: change });
    const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
    armed = true; native.insert("!");
    expect(() => {
      if (operation === "value") engine.setValue("stale owner value");
      else if (operation === "session") engine.setSession({ sessionId: "stale", value: "stale owner session" });
      else if (operation === "readonly") engine.setReadonly(true);
      else if (operation === "disabled") engine.setDisabled(true);
      else if (operation === "presentation") engine.setPresentation({ maxLines: 7 });
      else engine.releaseSession("a");
    }).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor operation was superseded by an owner update." }));
    expect(engine.getSessionId()).toBe("b"); expect(engine.getValue()).toBe("b"); expect(native.getReadOnly()).toBe(false);
    expect(native.textInput.getElement().disabled).toBe(false); expect(native.getOption("maxLines")).toBe(13); expect(report).not.toHaveBeenCalled();
    await Promise.resolve();
  }
});

test("beforeinput from an outgoing session is cancelled when flushing its edit changes owner context", () => {
  let armed = false;
  const { engine, host, callbacks } = setup({ value: "a", onChange: () => { if (armed) { armed = false; engine.setSession({ sessionId: "b", value: "b" }); } } });
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  armed = true; native.insert("!");
  const before = new InputEvent("beforeinput", { bubbles: true, cancelable: true, inputType: "insertText", data: "outgoing" });
  native.textInput.getElement().dispatchEvent(before);
  expect(before.defaultPrevented).toBe(true); expect(engine.getSessionId()).toBe("b"); expect(engine.getValue()).toBe("b");
  expect(callbacks.invalid).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "validation", message: "Code editor operation was superseded by an owner update." }));
});

test("explicit undefined presentation members preserve every configured component setting", () => {
  const control = createCodeEditor({ label: "Configured source", sessionId: "a", value: "a", theme: "dark", lineWrapping: true, showLineNumbers: false, maxLines: 12, required: true, invalid: "Keep this owner validation.", onChange: vi.fn(), report: vi.fn() });
  cleanup.push(control.dispose); document.body.append(control.node);
  const native = ace.edit(control.node.querySelector<HTMLElement>(".ace_editor")!), input = native.textInput.getElement();
  control.setPresentation({ theme: undefined, lineWrapping: undefined, showLineNumbers: undefined, maxLines: undefined, required: undefined, invalid: undefined });
  expect(control.node.querySelector<HTMLElement>(".code-editor-surface")!.dataset["codeTheme"]).toBe("dark");
  expect(native.session.getUseWrapMode()).toBe(true); expect(native.getOption("showGutter")).toBe(false); expect(native.getOption("maxLines")).toBe(12);
  expect(native.getOption("minLines")).toBe(5); expect(input.getAttribute("aria-required")).toBe("true"); expect(input.getAttribute("aria-invalid")).toBe("true");
  expect(control.node.querySelector(".code-editor-error")?.textContent).toBe("Keep this owner validation.");
});

const unavailableCommands = [
  ["find", "Find", { win: "Ctrl-F", mac: "Command-F" }],
  ["replace", "Replace", { win: "Ctrl-H", mac: "Command-Option-F" }],
  ["showSettingsMenu", "Settings", { win: "Ctrl-,", mac: "Command-," }],
  ["goToNextError", "Next error", { win: "Alt-E", mac: "F4" }],
  ["goToPreviousError", "Previous error", { win: "Alt-Shift-E", mac: "Shift-F4" }],
  ["gotoline", "Go to line", { win: "Ctrl-L", mac: "Command-L" }],
  ["openCommandPalette", "Command palette", { win: "F1", mac: "F1" }],
  ["openCommandPallete", "Command palette", undefined],
  ["modeSelect", "Language picker", { win: null, mac: null }],
] as const;
for (const [command, label, binding] of unavailableCommands) {
  test(`unavailable ${command} gives repeatable feedback without loading in editable and readonly source`, () => {
    const { engine, host, callbacks, report, onChange } = setup();
    const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
    const loader = vi.spyOn(ace.config, "loadModule");
    try {
      expect(native.commands.commands[command]?.bindKey).toEqual(binding);
      for (const readonly of [false, true]) {
        engine.setReadonly(readonly);
        for (let repetition = 0; repetition < 2; repetition++) {
          callbacks.status.mockClear();
          expect(() => native.commands.exec(command, native, undefined)).not.toThrow();
          expect(callbacks.status).toHaveBeenCalledExactlyOnceWith(`${label} is unavailable in this editor.`);
          expect(engine.getValue()).toBe("original");
          expect(engine.history.canUndo).toBe(false); expect(engine.history.canRedo).toBe(false);
        }
      }
      expect(loader).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled(); expect(onChange).not.toHaveBeenCalled();
    } finally { loader.mockRestore(); }
  });
}

test("history retains one group per flushed native batch rather than merging consecutive typing", async () => {
  const { engine, host } = setup({ value: "" });
  const native = ace.edit(host.querySelector<HTMLElement>(".ace_editor")!);
  native.insert("a"); await Promise.resolve();
  native.insert("b"); await Promise.resolve();
  native.insert("c"); await Promise.resolve();
  expect(engine.getValue()).toBe("abc");
  for (const value of ["ab", "a", ""]) { expect(engine.history.undo()).toBe(true); expect(engine.getValue()).toBe(value); }
  expect(engine.history.undo()).toBe(false);
  for (const value of ["a", "ab", "abc"]) { expect(engine.history.redo()).toBe(true); expect(engine.getValue()).toBe(value); }
  engine.setValue(""); native.insert("x"); native.insert("y"); await Promise.resolve();
  expect(engine.getValue()).toBe("xy"); expect(engine.history.undo()).toBe(true); expect(engine.getValue()).toBe(""); expect(engine.history.undo()).toBe(false);
});

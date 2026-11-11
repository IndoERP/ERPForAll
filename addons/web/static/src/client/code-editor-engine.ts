/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import ace from "ace-builds";
import "ace-builds/src-noconflict/mode-javascript";
import "ace-builds/src-noconflict/mode-xml";
import "ace-builds/src-noconflict/mode-scss";
import "ace-builds/src-noconflict/mode-python";
import "ace-builds/src-noconflict/mode-json";
import "ace-builds/src-noconflict/mode-sh";
import { ClientError } from "./transport.js";
import { CodeSessions, codeEditorLimits, codeEditorText, codePlainOptions, codeSessionId, type CodeSelection } from "./code-editor-session.js";
import { registerCodeQWebMode } from "./code-editor-qweb.js";
import type { CodeEditorController, CodeEditorHistory, CodeEditorOptions, CodeEditorPresentation, CodeEditorSessionOptions, CodeEngineCallbacks, CodeSessionId } from "./code-editor-types.js";

let configured = false;
const fail = (message: string): never => { throw new ClientError("validation", message); };
const boolean = (value: unknown, name: string): boolean => { if (typeof value !== "boolean") return fail(`Code editor ${name} must be a boolean.`); return value; };
const presentationKeys = ["theme", "lineWrapping", "showLineNumbers", "maxLines", "required", "invalid"] as const;
const optionKeys = [...presentationKeys, "label", "value", "sessionId", "mode", "readonly", "disabled", "limits", "signal", "onChange", "report", "onBlur"];
function presentation(value: CodeEditorPresentation): void {
  codePlainOptions(value, presentationKeys, "Code editor presentation option");
  if (value.theme !== undefined && !["auto", "light", "dark"].includes(value.theme)) fail("Code editor theme is not supported.");
  for (const name of ["lineWrapping", "showLineNumbers", "required"] as const) if (value[name] !== undefined) boolean(value[name], name);
  if (value.maxLines !== undefined && (!Number.isSafeInteger(value.maxLines) || value.maxLines < 1 || value.maxLines > 200)) fail("Code editor maxLines must be an integer from 1 to 200.");
  if (value.invalid !== undefined) codeEditorText(value.invalid, 4096);
}
function configure(): void {
  if (configured) return;
  ace.config.set("useStrictCSP", true);
  ace.config.set("packaged", false);
  // All admitted modes and the default textmate base are already bundled. A missing engine
  // module is an integration error, never permission to issue an implicit script request.
  ace.config.setLoader((name: string) => { throw new ClientError("unsupported", `Code editor module was not bundled: ${name}.`); });
  registerCodeQWebMode(); configured = true;
}
function detachNativeHistory(session: ace.Ace.EditSession): void {
  // The pinned runtime accepts null (and switches to its non-recording default manager), while
  // its declaration excludes it. This one narrow adapter prevents a second unbounded history.
  (session as unknown as { setUndoManager(manager: ace.Ace.UndoManager | null): void }).setUndoManager(null);
}

/** Private adapter. All user source/history admission belongs to CodeSessions; the engine only
 * supplies mature selection, keyboard, clipboard, IME and token rendering behavior. */
export function createCodeEngine(host: HTMLElement, options: CodeEditorOptions, callbacks: CodeEngineCallbacks): CodeEditorController {
  codePlainOptions(options, optionKeys, "Code editor option");
  if (typeof options.label !== "string" || !options.label.trim()) fail("Code editor label must be nonempty text.");
  codeEditorText(options.label, 4096);
  if (typeof options.onChange !== "function" || typeof options.report !== "function" || options.onBlur !== undefined && typeof options.onBlur !== "function") fail("Code editor requires onChange/report functions and an optional onBlur function.");
  if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) fail("Code editor signal must be an AbortSignal.");
  let readonly = options.readonly === undefined ? false : boolean(options.readonly, "readonly"), disabled = options.disabled === undefined ? false : boolean(options.disabled, "disabled");
  const settings: Required<CodeEditorPresentation> = { theme: options.theme === undefined ? "auto" : options.theme, lineWrapping: options.lineWrapping === undefined ? false : options.lineWrapping, showLineNumbers: options.showLineNumbers === undefined ? true : options.showLineNumbers, maxLines: options.maxLines === undefined ? 20 : options.maxLines, required: options.required === undefined ? false : options.required, invalid: options.invalid === undefined ? "" : options.invalid };
  presentation(settings);
  const sessions = new CodeSessions(codeEditorLimits(options.limits), message => callbacks.status(message));
  const surface = document.createElement("div"); surface.className = "code-editor-engine";
  let nativeEditor: ace.Ace.Editor | undefined;
  let dead = false, ignored = 0, pending: ace.Ace.EditSession | undefined, epoch = 0;
  // Pending native edits are admitted by flush. Meaningful owner operations replace this
  // token so an older operation cannot continue against their newer session/baseline/policy.
  // Identical value echoes intentionally keep the token and preserve pending undo behavior.
  let ownerContext: object = {};
  const nativeSessions = new Map<CodeSessionId, ace.Ace.EditSession>();
  const listeners = new Set<() => void>();
  const ownedSessions = new Set<ace.Ace.EditSession>();
  const detach: (() => void)[] = [];
  const destroySession = (session: ace.Ace.EditSession): void => {
    if (ownedSessions.delete(session)) session.destroy();
  };
  // This disposer exists before the first fallible initialization step. Register every
  // acquired listener/observer immediately, so construction failure shares normal teardown.
  const dispose = (): void => {
    if (dead) return;
    dead = true; ownerContext = {}; epoch++; pending = undefined; listeners.clear();
    const failures: unknown[] = [];
    const release = (action: () => void): void => { try { action(); } catch (error) { failures.push(error); } };
    for (const action of detach.splice(0).reverse()) release(action);
    // Pinned Ace destroys its attached session in Editor.destroy. Detach first so
    // this adapter remains the sole owner of both active and retained sessions.
    // The runtime accepts null although its declaration requires an EditSession.
    release(() => { if (nativeEditor) (nativeEditor as unknown as { setSession(session: ace.Ace.EditSession | null): void }).setSession(null); });
    release(() => { nativeEditor?.destroy(); });
    for (const session of ownedSessions) release(() => destroySession(session));
    nativeSessions.clear();
    release(() => sessions.dispose()); release(() => surface.remove());
    if (failures.length) throw new AggregateError(failures, "Code editor cleanup failed.");
  };
  try {
    sessions.activate({ sessionId: options.sessionId, value: options.value, mode: options.mode === undefined ? "text" : options.mode });
    configure(); host.append(surface);
    const editor = ace.edit(surface, { useWorker: false, showPrintMargin: false, enableKeyboardAccessibility: true, useSoftTabs: true, tabSize: 2 });
    nativeEditor = editor;
    const originalSession = editor.session; ownedSessions.add(originalSession);
    const input = editor.textInput.getElement();
    const alive = (): void => { if (dead) fail("Code editor is disposed."); };
    const editable = (): void => { alive(); if (readonly) fail("Code editor is readonly."); if (disabled) fail("Code editor is disabled."); };
    const selection = (): CodeSelection => {
      const cursor = editor.getCursorPosition(), anchor = editor.selection.getSelectionAnchor();
      return { row: cursor.row, column: cursor.column, anchorRow: anchor.row, anchorColumn: anchor.column };
    };
    const restoreSelection = (): void => {
      const selected = sessions.selection;
      editor.selection.setSelectionAnchor(selected.anchorRow ?? selected.row, selected.anchorColumn ?? selected.column);
      editor.selection.selectTo(selected.row, selected.column);
    };
    const notify = (): void => {
      if (dead) return;
      // Commit presentation before public subscribers run: a nested owner update publishes
      // its newer snapshot and no outer wrapper continuation may overwrite it.
      callbacks.state({ readonly, disabled, presentation: { ...settings } });
      for (const listener of [...listeners]) { if (dead) break; try { listener(); } catch (error) { options.report(error); } }
    };
    const change = (id: CodeSessionId, value: string): void => { if (!dead) try { options.onChange({ sessionId: id, value }); } catch (error) { options.report(error); } };
    const renderDocument = (): void => {
      if (dead) return;
      const current = nativeSessions.get(sessions.id);
      if (!current) throw new ClientError("server", "Code editor native session is missing.");
      ignored++;
      try { if (current.getValue() !== sessions.value) current.setValue(sessions.value); detachNativeHistory(current); restoreSelection(); }
      finally { ignored--; }
    };
    const flush = (): void => {
      const current = pending; pending = undefined;
      if (!current || dead || current !== editor.session) return;
      const id = sessions.id;
      try {
        if (readonly || disabled) { renderDocument(); notify(); return; }
        const changed = sessions.edit(current.getValue(), selection()), value = sessions.value;
        notify(); if (changed) change(id, value);
      } catch (error) {
        renderDocument(); notify();
        if (error instanceof ClientError && error.kind === "validation") callbacks.invalid(error); else options.report(error);
      }
    };
    const flushOperation = (): void => {
      const identity = ownerContext; flush();
      if (identity !== ownerContext) fail("Code editor operation was superseded by an owner update.");
      alive();
    };
    const onNativeChange = (): void => {
      if (dead || ignored) return;
      if (!pending) { pending = editor.session; const identity = epoch; queueMicrotask(() => { if (!dead && identity === epoch) flush(); }); }
    };
    const saveSelection = (): void => { if (!dead && !ignored && !pending) sessions.selection = selection(); };
    const apply = (): void => {
      host.dataset["codeMode"] = sessions.mode; host.dataset["codeReadonly"] = String(readonly); host.dataset["codeDisabled"] = String(disabled); host.dataset["codeTheme"] = settings.theme;
      editor.setReadOnly(readonly || disabled);
      editor.setOptions({ highlightActiveLine: !readonly && !disabled, highlightGutterLine: !readonly && !disabled, displayIndentGuides: !readonly && !disabled, showGutter: settings.showLineNumbers && !readonly, wrap: settings.lineWrapping, maxLines: settings.maxLines, minLines: Math.min(settings.maxLines, 5) });
      const cursorLayer = surface.querySelector<HTMLElement>(".ace_cursor-layer"), gutter = surface.querySelector<HTMLElement>(".ace_gutter");
      if (!cursorLayer || !gutter) throw new ClientError("server", "The pinned code editor rendering layers are missing.");
      cursorLayer.style.display = readonly || disabled ? "none" : "block";
      input.disabled = disabled; input.tabIndex = -1;
      editor.renderer.scroller.tabIndex = disabled ? -1 : 0;
      gutter.tabIndex = -1;
      editor.resize();
    };
    const activate = (): void => {
      let current = nativeSessions.get(sessions.id);
      if (!current) {
        current = new ace.EditSession(sessions.value); ownedSessions.add(current); nativeSessions.set(sessions.id, current);
        current.setUseWorker(false); detachNativeHistory(current); current.setOptions({ tabSize: 2, useSoftTabs: true });
      }
      ignored++;
      try {
        if (current.getValue() !== sessions.value) current.setValue(sessions.value);
        detachNativeHistory(current);
        current.setMode(`ace/mode/${sessions.mode === "bash" ? "sh" : sessions.mode}`);
        editor.setSession(current); restoreSelection(); apply();
      } finally { ignored--; }
    };
    const history: CodeEditorHistory = {
      get canUndo() { return !dead && !readonly && !disabled && sessions.canUndo; },
      get canRedo() { return !dead && !readonly && !disabled && sessions.canRedo; },
      undo() { editable(); const identity = ownerContext; flush(); if (identity !== ownerContext) return false; editable(); const changed = sessions.undo(); if (changed) { ownerContext = {}; const id = sessions.id, value = sessions.value; renderDocument(); notify(); change(id, value); } return changed; },
      redo() { editable(); const identity = ownerContext; flush(); if (identity !== ownerContext) return false; editable(); const changed = sessions.redo(); if (changed) { ownerContext = {}; const id = sessions.id, value = sessions.value; renderDocument(); notify(); change(id, value); } return changed; },
      subscribe(listener) { alive(); if (typeof listener !== "function") fail("Code editor history listener must be a function."); listeners.add(listener); return () => { listeners.delete(listener); }; },
    };
    // Pinned Ace defaults load optional extensions. Preserve their platform bindings but
    // make unavailable actions explicit, including repeated presses and readonly source.
    for (const [name, label] of [
      ["find", "Find"], ["replace", "Replace"], ["showSettingsMenu", "Settings"],
      ["goToNextError", "Next error"], ["goToPreviousError", "Previous error"],
      ["gotoline", "Go to line"], ["openCommandPalette", "Command palette"],
      ["openCommandPallete", "Command palette"], ["modeSelect", "Language picker"],
    ] as const) {
      const command = editor.commands.commands[name];
      if (!command) throw new ClientError("server", `The pinned code editor command is missing: ${name}.`);
      editor.commands.addCommand({ ...command, readOnly: true, exec: () => callbacks.status(`${label} is unavailable in this editor.`) });
    }
    editor.commands.addCommand({ name: "undo", bindKey: { win: "Ctrl-Z", mac: "Command-Z" }, exec: () => { try { history.undo(); } catch (error) { if (error instanceof ClientError && error.kind === "validation") callbacks.invalid(error); else options.report(error); } }, readOnly: false });
    editor.commands.addCommand({ name: "redo", bindKey: { win: "Ctrl-Shift-Z|Ctrl-Y", mac: "Command-Shift-Z" }, exec: () => { try { history.redo(); } catch (error) { if (error instanceof ClientError && error.kind === "validation") callbacks.invalid(error); else options.report(error); } }, readOnly: false });
    detach.push(() => { editor.off("change", onNativeChange); }); editor.on("change", onNativeChange);
    const onBlur = (): void => { const identity = ownerContext; flush(); if (!dead && identity === ownerContext && options.onBlur) try { options.onBlur(); } catch (error) { options.report(error); } };
    detach.push(() => { editor.off("blur", onBlur); }); editor.on("blur", onBlur);
    const preflight = (event: Event, text: string | null): void => {
      if (dead || readonly || disabled) { event.preventDefault(); event.stopImmediatePropagation(); return; }
      if (text === null) return; // Composition and browser-only input without exposed data use final change admission.
      try {
        flushOperation(); editable();
        sessions.admitInsertion(text, editor.session.getTextRange(editor.getSelectionRange()));
      } catch (error) {
        event.preventDefault(); event.stopImmediatePropagation();
        if (error instanceof ClientError && error.kind === "validation") callbacks.invalid(error); else options.report(error);
      }
    };
    const onPaste = (event: ClipboardEvent): void => { preflight(event, event.clipboardData?.getData("text/plain") ?? null); };
    const onDrop = (event: DragEvent): void => { preflight(event, event.dataTransfer?.getData("text/plain") ?? null); };
    const onBeforeInput = (event: InputEvent): void => { if (event.inputType.startsWith("insert")) preflight(event, event.data); };
    detach.push(() => { host.removeEventListener("paste", onPaste, true); host.removeEventListener("drop", onDrop, true); host.removeEventListener("beforeinput", onBeforeInput, true); });
    host.addEventListener("paste", onPaste, true); host.addEventListener("drop", onDrop, true); host.addEventListener("beforeinput", onBeforeInput, true);
    activate(); destroySession(originalSession);
    // Editor selection objects change with sessions. A switch saves the full outgoing selection;
    // the current editor event additionally records cursor moves without document edits.
    detach.push(() => { editor.off("changeSelection", saveSelection); }); editor.on("changeSelection", saveSelection);
    if (typeof ResizeObserver !== "undefined") {
      const observer = new ResizeObserver(() => { if (!dead) editor.resize(); });
      detach.push(() => { observer.disconnect(); }); observer.observe(host);
    }
    const controller: CodeEditorController = {
      history, getValue() { alive(); flush(); alive(); return sessions.value; }, getSessionId() { alive(); return sessions.id; },
      setValue(value) { alive(); flushOperation(); saveSelection(); const previous = sessions.value; sessions.replace(value); if (sessions.value !== previous) ownerContext = {}; renderDocument(); notify(); },
      setSession(value: CodeEditorSessionOptions) { alive(); flushOperation(); saveSelection(); const previous = { id: sessions.id, value: sessions.value, mode: sessions.mode }; sessions.activate(value); if (previous.id !== sessions.id || previous.value !== sessions.value || previous.mode !== sessions.mode) ownerContext = {}; epoch++; pending = undefined; activate(); notify(); },
      setReadonly(value) { alive(); flushOperation(); const selected = boolean(value, "readonly"); if (selected !== readonly) ownerContext = {}; readonly = selected; apply(); notify(); },
      setDisabled(value) { alive(); flushOperation(); const selected = boolean(value, "disabled"); if (selected !== disabled) ownerContext = {}; disabled = selected; apply(); notify(); },
      setPresentation(value) { alive(); presentation(value); flushOperation(); for (const key of presentationKeys) { const selected = value[key]; if (selected !== undefined && selected !== settings[key]) { ownerContext = {}; Object.assign(settings, { [key]: selected }); } } apply(); notify(); },
      focus() { alive(); if (!disabled) editor.focus(); },
      releaseSession(value) { alive(); flushOperation(); const id = codeSessionId(value); sessions.release(id); ownerContext = {}; const native = nativeSessions.get(id); if (native) destroySession(native); nativeSessions.delete(id); notify(); },
      dispose,
    };
    detach.push(() => { options.signal?.removeEventListener("abort", dispose); });
    options.signal?.addEventListener("abort", dispose, { once: true });
    if (options.signal?.aborted) dispose(); else notify();
    return controller;
  } catch (error) {
    try { dispose(); }
    catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "Code editor construction and cleanup failed.", { cause: error });
    }
    throw error;
  }
}

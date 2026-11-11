/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { createCodeEngine } from "./code-editor-engine.js";
import type { CodeEditorController, CodeEditorHandle, CodeEditorOptions, CodeEditorPresentation } from "./code-editor-types.js";
import { ClientError } from "./transport.js";

let codeEditorIdentity = 0;

/** An owner-scoped source editor. Values are text; the caller owns saving and validation.
 * Synchronous option/update errors propagate. User admission errors are shown inline; unrelated
 * owner callback failures use `report`. Dispose this handle with the owning screen or AbortSignal. */
export function createCodeEditor(options: CodeEditorOptions): CodeEditorHandle {
  const identity = `code-editor-${++codeEditorIdentity}`;
  const node = el("section", undefined, "code-editor"), heading = el("div", undefined, "code-editor-heading");
  const label = el("label", undefined, "code-editor-label"), caption = el("span"), required = el("span", "*", "code-editor-required");
  const toolbar = el("div", undefined, "code-editor-history"), undo = el("button", "Undo"), redo = el("button", "Redo");
  const host = el("div", undefined, "code-editor-surface");
  const hint = el("small", "Press Enter to edit. Press Escape, then Tab to leave the editor.", "code-editor-hint");
  const error = el("small", undefined, "code-editor-error"), status = el("small", undefined, "code-editor-status");
  label.id = `${identity}-label`; required.setAttribute("aria-hidden", "true"); label.append(caption, required);
  toolbar.setAttribute("role", "group"); toolbar.setAttribute("aria-label", "Source history");
  undo.type = "button"; redo.type = "button"; undo.disabled = true; redo.disabled = true; toolbar.append(undo, redo);
  hint.id = `${identity}-hint`; error.id = `${identity}-error`; error.setAttribute("role", "status");
  status.id = `${identity}-status`; status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  error.hidden = true; status.hidden = true;
  heading.append(label, toolbar); node.append(heading, host, hint, error, status);

  let controller: CodeEditorController | undefined, input: HTMLTextAreaElement | undefined, dead = false;
  let readonly = false, disabled = false;
  let presentation: CodeEditorPresentation = {};
  let admissionError = "", ownerError = "";
  const displayError = (): void => {
    const text = admissionError || ownerError;
    error.textContent = text; error.hidden = text === "";
    node.classList.toggle("code-editor-invalid", text !== "");
    if (text) input?.setAttribute("aria-invalid", "true"); else input?.removeAttribute("aria-invalid");
  };
  const refresh = (): void => {
    if (dead || !controller) return;
    undo.disabled = !controller.history.canUndo; redo.disabled = !controller.history.canRedo;
    node.classList.toggle("code-editor-readonly", readonly); node.classList.toggle("code-editor-disabled", disabled);
    node.setAttribute("aria-disabled", String(disabled));
    hint.textContent = disabled ? "Editing is disabled." : readonly ? "Read only. Select text to copy." : "Press Enter to edit. Press Escape, then Tab to leave the editor.";
    input?.setAttribute("aria-readonly", String(readonly));
  };
  const showPresentation = (): void => {
    node.dataset["codeTheme"] = presentation.theme ?? "auto";
    required.hidden = presentation.required !== true;
    node.classList.toggle("code-editor-required-value", presentation.required === true);
    input?.setAttribute("aria-required", String(presentation.required === true));
    displayError(); refresh();
  };
  // The engine validates all options before creating its editing surface. Its controller remains
  // private; the surrounding component is only responsible for labels, controls and feedback.
  // A pre-aborted owner is disposed synchronously after the wrapper associates its native
  // label. Passing only this verified signal as absent prevents engine destruction from
  // removing the input before that association; malformed signals still reach admission.
  const alreadyAborted = options?.signal instanceof AbortSignal && options.signal.aborted;
  const engineOptions = alreadyAborted ? { ...options, signal: undefined } : options;
  controller = createCodeEngine(host, engineOptions, {
    state: snapshot => {
      if (dead) return;
      readonly = snapshot.readonly; disabled = snapshot.disabled;
      presentation = { ...snapshot.presentation }; ownerError = snapshot.presentation.invalid;
      admissionError = ""; showPresentation();
    },
    invalid: failure => { if (!dead) { admissionError = failure.message; displayError(); refresh(); } },
    status: message => { if (!dead) { status.textContent = message; status.hidden = message === ""; } },
  });
  input = host.querySelector<HTMLTextAreaElement>("textarea") ?? undefined;
  if (!input) {
    controller.dispose();
    throw new ClientError("server", "Code editor input was not created.");
  }
  caption.textContent = options.label;
  node.setAttribute("aria-labelledby", label.id);
  input.id = `${identity}-input`; label.htmlFor = input.id;
  input.setAttribute("aria-label", options.label); input.setAttribute("aria-labelledby", label.id);
  input.setAttribute("aria-describedby", `${hint.id} ${error.id} ${status.id}`);
  const scroller = host.querySelector<HTMLElement>(".ace_scroller");
  scroller?.setAttribute("aria-labelledby", label.id); scroller?.setAttribute("aria-describedby", hint.id);
  const unsubscribe = controller.history.subscribe(refresh);
  const run = (job: () => unknown): void => {
    if (dead) return;
    try { job(); } catch (failure) { options.report(failure); }
  };
  const onUndo = (): void => run(() => controller!.history.undo());
  const onRedo = (): void => run(() => controller!.history.redo());
  undo.addEventListener("click", onUndo); redo.addEventListener("click", onRedo);
  const dispose = (): void => {
    if (dead) return;
    dead = true;
    undo.removeEventListener("click", onUndo); redo.removeEventListener("click", onRedo);
    options.signal?.removeEventListener("abort", dispose); unsubscribe();
    undo.disabled = true; redo.disabled = true; node.classList.add("code-editor-disposed");
    controller!.dispose();
  };
  options.signal?.addEventListener("abort", dispose, { once: true });
  showPresentation();
  if (options.signal?.aborted) dispose();
  return {
    node,
    history: controller.history,
    getValue: () => controller!.getValue(),
    getSessionId: () => controller!.getSessionId(),
    // The private state notification is authoritative and occurs before any public subscriber.
    // These delegates deliberately perform no local work after a potentially reentrant call.
    setValue: value => controller!.setValue(value),
    setSession: session => controller!.setSession(session),
    setReadonly: value => controller!.setReadonly(value),
    setDisabled: value => controller!.setDisabled(value),
    setPresentation: value => controller!.setPresentation(value),
    focus: () => controller!.focus(),
    releaseSession: id => controller!.releaseSession(id),
    dispose,
  };
}

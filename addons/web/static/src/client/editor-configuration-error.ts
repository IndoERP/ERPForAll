/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { assertEditorValue, type DateKind, type EditorValue } from "./datetime-values.js";
import { ClientError } from "./transport.js";

interface ConfigurationErrorOptions {
  kind: DateKind | "time"; name: string; label: string; value: EditorValue;
  required?: boolean; readonly?: boolean; signal?: AbortSignal;
  onInvalid?(error: ClientError): void; report(error: unknown): void;
}
let configurationErrorIdentity = 0;

/** An unavailable editor keeps the owner's canonical value and exposes the exact admission error.
 * No input, window or signal listeners are attached: an unavailable control cannot edit or open an overlay. */
export function editorConfigurationError(options: ConfigurationErrorOptions, failure: ClientError): {
  node: HTMLElement; input: HTMLInputElement; setValue(value: EditorValue): void; dispose(): void;
} {
  const node = el("div", undefined, `field-label ${options.kind === "time" ? "time-editor" : "datetime-editor"}`);
  const label = el("label", options.label), input = el("input"), error = el("small", failure.message, "editor-validation");
  const identity = `editor-configuration-error-${++configurationErrorIdentity}`;
  label.id = `${identity}-label`; label.htmlFor = identity; input.id = identity; input.name = options.name;
  input.type = "text"; input.disabled = true; input.required = options.required ?? false;
  input.value = options.value === false ? "" : options.value;
  input.title = "Stored value (unformatted)";
  input.setAttribute("aria-labelledby", label.id); input.setAttribute("aria-invalid", "true");
  error.id = `${identity}-error`; error.setAttribute("role", "status");
  const note = el("small", "Stored value is shown without formatting.");
  input.setAttribute("aria-describedby", error.id);
  if (options.required) {
    node.classList.add("field-required"); const marker = el("span", "*", "field-required-marker");
    marker.setAttribute("aria-hidden", "true"); label.append(" ", marker);
  }
  if (options.readonly) node.classList.add("field-readonly");
  node.append(label, input, error, note);
  let dead = options.signal?.aborted === true;
  if (!dead) {
    if (options.onInvalid) {
      try { options.onInvalid(failure); } catch (callbackFailure) { options.report(callbackFailure); }
    } else options.report(failure);
  }
  return { node, input, setValue: value => {
    if (dead || options.signal?.aborted) throw new ClientError("validation", options.kind === "time" ? "Time editor is disposed." : "Date/time editor is disposed.");
    assertEditorValue(options.kind, value); input.value = value === false ? "" : value;
  }, dispose: () => { dead = true; } };
}

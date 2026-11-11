/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createCodeEditor } from "./code-editor.js";
import { codeEditorMode, codeEditorText, CODE_EDITOR_HARD_LIMITS } from "./code-editor-session.js";
import type { Field } from "./metadata.js";
import type { WidgetEnvironment, WidgetHandle } from "./registry.js";
import { ClientError } from "./transport.js";

let identity = 0;
function source(value: unknown, field: Field): string {
  if (value === false || value === null || value === undefined) return "";
  if (typeof value !== "string") throw new ClientError("validation", `Code field ${field.label} must contain text.`);
  return value;
}
/** Original text-field adapter. RecordForm owns its draft, validation and commit lifecycle. */
export function renderCodeField(env: WidgetEnvironment): WidgetHandle {
  let field = env.field, dead = false;
  const initial = source(env.value, field);
  if (field.kind !== "text") throw new ClientError("validation", "The code widget requires a text field.");
  const mode = codeEditorMode(field.codeMode);
  if (!env.draft) throw new ClientError("validation", "Code fields require the RecordForm draft lifecycle.");
  const draft = env.draft;
  let validating = false;
  const editor = createCodeEditor({ label: field.label, value: initial, mode, sessionId: `field-${++identity}`,
    readonly: field.readonly, required: field.required, signal: env.signal, report: env.report,
    onChange: ({ value }) => {
      if (dead) return;
      draft.stage(value === "" ? false : value);
      if (validating) editor.setPresentation({ invalid: field.required && !field.readonly && value === "" ? `${field.label} is required.` : "" });
    },
    onBlur: () => { if (!dead) draft.commit(); },
  });
  const input = editor.node.querySelector("textarea");
  if (!input) { editor.dispose(); throw new ClientError("server", "Code field input was not created."); }
  input.name = field.name;
  editor.node.classList.add("field-label", "code-field");
  return { node: editor.node, dispose: () => { if (dead) return; dead = true; editor.dispose(); }, draft: {
    admit: value => { codeEditorText(source(value, field), CODE_EDITOR_HARD_LIMITS.maxDocumentBytes); },
    flush: () => { editor.getValue(); },
    update: (value, next) => { field = next; editor.setValue(source(value, field)); editor.setReadonly(field.readonly); editor.setPresentation({ required: field.required }); },
    validate: () => {
      validating = true;
      const invalid = field.required && !field.readonly && editor.getValue() === "";
      editor.setPresentation({ invalid: invalid ? `${field.label} is required.` : "" });
      return !invalid;
    },
    focus: editor.focus,
    setDisabled: editor.setDisabled,
  } };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { createHtmlEditor } from "./html-editor.js";
import { admitHtml, htmlEditorLimits } from "./html-editor-document.js";
import type { MessageServices } from "./messages.js";
import type { Field } from "./metadata.js";
import type { WidgetEnvironment, WidgetHandle } from "./registry.js";
import { ClientError } from "./transport.js";

function source(value: unknown, field: Field): string {
  if (value === false || value === null || value === undefined) return "";
  if (typeof value !== "string") throw new ClientError("validation", `Html field ${field.label} must contain text.`);
  return value;
}

/** Form HTML field adapter over the merged HTML editor. RecordForm owns draft, validation and commit lifecycle. */
export function renderHtmlField(env: WidgetEnvironment): WidgetHandle {
  if (env.viewContext === "list") return renderListHtmlField(env);
  let field = env.field;
  let dead = false;
  if (field.kind !== "html") throw new ClientError("validation", "The html widget requires an html field.");
  if (!env.draft) throw new ClientError("validation", "Html fields require the RecordForm draft lifecycle.");
  const draft = env.draft;
  let validating = false;
  const initial = source(env.value, field);

  // The editor opens link popovers and dialogs and reports its own failures: a host that gives neither cannot host it.
  if (!env.messages) throw new ClientError("unsupported", "The html widget requires the host message services (popovers and dialogs).");
  if (typeof env.report !== "function") throw new ClientError("unsupported", "The html widget requires an error reporter from its host.");
  const messages: Pick<MessageServices, "popovers" | "dialogs"> = env.messages;

  const editor = createHtmlEditor({
    environment: {
      messages,
      report: env.report,
      ...(env.signal ? { signal: env.signal } : {}),
    },
    label: field.label,
    value: initial,
    readonly: field.readonly,
    required: field.required,
    change: (html: string) => {
      if (dead) return;
      draft.stage(html === "" ? false : html);
      if (validating) {
        const invalid = field.required && !field.readonly && html === "";
        editor.setInvalid(invalid ? `${field.label} is required.` : "");
      }
    },
  });

  const onFocusOut = (event: FocusEvent): void => {
    if (dead) return;
    if (event.relatedTarget instanceof Node && editor.node.contains(event.relatedTarget)) {
      return;
    }
    draft.commit();
  };
  editor.node.addEventListener("focusout", onFocusOut);

  editor.node.classList.add("field-label", "html-field");
  if (field.readonly) editor.node.classList.add("field-readonly");

  const dispose = (): void => {
    if (dead) return;
    dead = true;
    editor.node.removeEventListener("focusout", onFocusOut);
    editor.dispose();
  };

  return {
    node: editor.node,
    dispose,
    draft: {
      admit: value => {
        const text = source(value, field);
        admitHtml(document, text, htmlEditorLimits(), "initial");
      },
      flush: () => {
        editor.exportHtml();
      },
      update: (value, next) => {
        field = next;
        editor.setValue(source(value, field));
        editor.setReadonly(field.readonly);
        editor.setRequired(field.required);
        editor.node.classList.toggle("field-readonly", field.readonly);
      },
      validate: () => {
        validating = true;
        const html = editor.exportHtml();
        const invalid = field.required && !field.readonly && html === "";
        editor.setInvalid(invalid ? `${field.label} is required.` : "");
        return !invalid;
      },
      focus: () => editor.focus(),
      setDisabled: disabled => editor.setDisabled(disabled),
    },
  };
}

/** List cells have no RecordForm draft lifecycle: they display safe read-only plain text. */
export function renderListHtmlField(env: WidgetEnvironment): WidgetHandle {
  const value = env.value;
  if (value !== false && value !== null && value !== undefined && typeof value !== "string") {
    throw new ClientError("server", `Html field ${env.field.label} must contain text.`);
  }
  let text = "";
  if (typeof value === "string" && value !== "") {
    const template = document.createElement("template");
    template.innerHTML = value;
    template.content.querySelectorAll("script,style").forEach(s => s.remove());
    text = template.content.textContent ?? "";
  }
  const node = el("span", text, "field-read-value list-html");
  node.setAttribute("aria-label", env.field.label);
  return { node, dispose: () => {} };
}

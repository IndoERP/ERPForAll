/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import { safeLink } from "./field-scalars.js";
import { ClientError } from "./transport.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

/** Accepted field kind per copy widget; the server admits the same pairs (`addons/web/src/lib.rs`). */
const COPY_WIDGET_KINDS = { CopyClipboardChar: "char", CopyClipboardText: "text", CopyClipboardURL: "char" } as const;
type CopyWidget = keyof typeof COPY_WIDGET_KINDS;
/** How long the "Copied" confirmation stays visible. */
const CONFIRMATION_MS = 800;

/** The reason text of a clipboard rejection; a DOMException need not be an `Error` instance in every realm. */
function errorMessage(err: unknown): string {
  if (typeof err === "object" && err !== null && "message" in err && typeof err.message === "string" && err.message) {
    return err.message;
  }
  return String(err);
}

/**
 * Copy-to-clipboard field widgets: the value (a safe link for `CopyClipboardURL` in read mode)
 * or an input/textarea in edit mode, plus a Copy button that is hidden while the value is empty. A successful copy
 * shows a short confirmation; a missing or refused clipboard shows the error in the widget's status message, and the
 * next copy attempt or edit clears it. The copy reads the current text, including an unsaved draft.
 */
export function copyClipboardControl(options: FieldControlOptions): FieldControl {
  const { field, value, change } = options;
  const config = field.options ?? {};
  const widget = (field.widget ?? "CopyClipboardChar") as CopyWidget;
  const kind = COPY_WIDGET_KINDS[widget];
  if (kind === undefined) throw new ClientError("server", `Unknown copy widget ${String(field.widget)} on ${field.name}.`);
  // The server refuses these pairs at view admission; this guard covers client-built metadata.
  if (field.kind !== kind) throw new ClientError("server", `Field ${field.name} with ${widget} widget must be a ${kind} field.`);

  const rawValue = value === null || value === undefined || value === false ? "" : String(value);
  let currentValue = rawValue;
  const editable = !field.readonly;
  const input = editable ? (widget === "CopyClipboardText" ? el("textarea") : el("input")) : null;

  const container = el("div", undefined, `field-copy-clipboard o_field_${widget}`);
  // In edit mode the input is the labelled control; in read mode the container is a labelled group.
  const shell = fieldShell(field, input ?? container, options.viewContext);

  const feedback = el("span", undefined, "field-copy-feedback");
  feedback.setAttribute("role", "status");
  feedback.setAttribute("aria-live", "polite");
  feedback.hidden = true;
  let feedbackTimer: ReturnType<typeof setTimeout> | null = null;
  const clearFeedback = () => {
    if (feedbackTimer) clearTimeout(feedbackTimer);
    feedbackTimer = null;
    feedback.hidden = true;
    feedback.textContent = "";
    feedback.className = "field-copy-feedback";
  };
  const showFeedback = (text: string, outcome: "success" | "error") => {
    clearFeedback();
    feedback.textContent = text;
    feedback.className = `field-copy-feedback field-copy-${outcome}`;
    feedback.hidden = false;
    if (outcome === "success") feedbackTimer = setTimeout(clearFeedback, CONFIRMATION_MS);
  };

  const copyBtn = button("Copy", async () => {
    const text = currentValue;
    if (!text) return;
    clearFeedback();
    try {
      if (!navigator.clipboard || typeof navigator.clipboard.writeText !== "function") {
        throw new Error("the clipboard is not available in this browser context");
      }
      await navigator.clipboard.writeText(text);
      showFeedback("Copied", "success");
    } catch (err: unknown) {
      showFeedback(`Clipboard copy failed: ${errorMessage(err)}`, "error");
    }
  }, `field-copy-button o_clipboard_button o_btn_${widget === "CopyClipboardText" ? "text" : widget === "CopyClipboardURL" ? "url" : "char"}_copy`);
  copyBtn.type = "button";
  copyBtn.setAttribute("aria-label", `Copy ${field.label}`);
  const syncButton = () => {
    copyBtn.hidden = currentValue.trim() === "";
    if (copyBtn.hidden) clearFeedback();
  };
  syncButton();
  const dispose = () => clearFeedback();

  if (input === null) {
    const href = widget === "CopyClipboardURL" && currentValue ? safeLink("url", currentValue) : null;
    let shown: HTMLElement;
    if (href) {
      const link = el("a", currentValue, "field-link");
      link.href = href;
      link.rel = "noopener noreferrer";
      link.target = "_blank";
      shown = link;
    } else {
      shown = el("span", currentValue, "field-read-value");
    }
    container.append(shown, copyBtn, feedback);
    return { node: shell.node, dispose };
  }

  input.name = field.name;
  input.required = field.required;
  if (config.placeholder !== undefined) input.placeholder = config.placeholder;
  input.value = currentValue;
  input.addEventListener("input", () => {
    currentValue = input.value;
    syncButton();
    clearFeedback();
    shell.clear();
  });
  input.addEventListener("change", () => {
    currentValue = input.value;
    syncButton();
    if (!input.checkValidity()) {
      shell.invalid(input.validationMessage);
      return;
    }
    shell.clear();
    change(input.value === "" ? false : input.value);
  });
  // The shell placed the input in its own node; move it into the widget row next to the button.
  container.append(input, copyBtn, feedback);
  shell.node.insertBefore(container, shell.node.querySelector(".field-error"));

  return {
    node: shell.node,
    dispose,
    captureDraft: () => input.value !== rawValue || !input.validity.valid ? { value: input.value, message: input.validationMessage } : undefined,
    restoreDraft: draft => {
      input.value = draft.value;
      currentValue = draft.value;
      syncButton();
      if (draft.message && input.validity.valid) shell.invalid(draft.message);
    },
  };
}

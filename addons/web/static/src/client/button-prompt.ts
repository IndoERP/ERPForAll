/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import type { DialogService } from "./dialog-service.js";
import type { ButtonPrompt } from "./metadata.js";
import { ClientError } from "./transport.js";

/** What the user answered: the `prompt_value` sent to the method, `null` for an empty optional answer. */
export interface PromptAnswer { readonly value: string | null }
export interface ButtonPromptOptions {
  /** The button's caption: the dialog title and the confirming action. */
  readonly title: string;
  readonly prompt: ButtonPrompt;
  readonly service: DialogService;
  readonly signal?: AbortSignal;
}

const ISO_DATE = /^(\d{4})-(\d{2})-(\d{2})$/;
/** Whether `value` is a real calendar date written `YYYY-MM-DD`. */
export function isoDate(value: string): boolean {
  const match = ISO_DATE.exec(value);
  if (!match) return false;
  const [year, month, day] = [Number(match[1]), Number(match[2]), Number(match[3])];
  const date = new Date(Date.UTC(year, month - 1, day));
  return date.getUTCFullYear() === year && date.getUTCMonth() === month - 1 && date.getUTCDate() === day;
}

/**
 * Check one answer against the button's declaration: required, length, date and choice. Returns the value sent
 * (`null` for an empty optional answer) or throws a `validation` error naming the prompt's label.
 */
export function promptValue(prompt: ButtonPrompt, raw: string): string | null {
  const value = prompt.kind === "text" ? raw.trim() : raw;
  if (value === "") {
    if (prompt.required) throw new ClientError("validation", `${prompt.label} is required.`);
    return null;
  }
  if (prompt.kind === "text" && prompt.max_length !== undefined && [...value].length > prompt.max_length) throw new ClientError("validation", `${prompt.label} can be up to ${prompt.max_length} characters.`);
  if (prompt.kind === "date" && !isoDate(value)) throw new ClientError("validation", `${prompt.label} must be a date.`);
  if (prompt.kind === "selection" && !prompt.choices?.some(choice => choice.value === value)) throw new ClientError("validation", `${prompt.label} must be one of the listed choices.`);
  return value;
}

let identity = 0;
function promptControl(prompt: ButtonPrompt): HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement {
  if (prompt.kind === "text") {
    const area = el("textarea", undefined, "button-prompt-input"); area.rows = 3;
    if (prompt.max_length !== undefined) area.maxLength = prompt.max_length;
    return area;
  }
  if (prompt.kind === "date") { const input = el("input", undefined, "button-prompt-input"); input.type = "date"; return input; }
  const select = el("select", undefined, "button-prompt-input");
  if (!prompt.required) select.append(new Option("", ""));
  for (const choice of prompt.choices ?? []) select.append(new Option(choice.label, choice.value));
  if (prompt.required) select.selectedIndex = -1;
  return select;
}

/**
 * Ask the button's question in a modal dialog. Resolves with the answer when the user confirms a valid one, or
 * `undefined` when the dialog is cancelled, dismissed or its owner goes away; the method is then not called.
 */
export function askButtonPrompt(options: ButtonPromptOptions): Promise<PromptAnswer | undefined> {
  const { prompt, service } = options;
  if (options.signal?.aborted) return Promise.resolve(undefined);
  const id = `button-prompt-${++identity}`;
  const body = el("div", undefined, "button-prompt");
  const label = el("label", prompt.label); label.htmlFor = id;
  if (prompt.required) { const marker = el("span", " *", "field-required-marker"); marker.setAttribute("aria-hidden", "true"); label.append(marker); }
  const control = promptControl(prompt);
  control.id = id; control.required = prompt.required; control.autofocus = true;
  const error = el("span", undefined, "field-error"); error.id = `${id}-error`; error.setAttribute("role", "status"); error.hidden = true;
  body.append(label, control, error);
  const footer = el("div", undefined, "actions");
  return new Promise(resolve => {
    let settled = false;
    const finish = (answer: PromptAnswer | undefined): void => { if (settled) return; settled = true; handle.close(); resolve(answer); };
    const accept = (): void => {
      try {
        const value = promptValue(prompt, control.value);
        finish({ value });
      } catch (failure) {
        if (!(failure instanceof ClientError) || failure.kind !== "validation") throw failure;
        error.textContent = failure.message; error.hidden = false;
        control.setAttribute("aria-invalid", "true"); control.setAttribute("aria-describedby", error.id); control.focus();
      }
    };
    const confirm = button(options.title, accept, "primary"); confirm.dataset["defaultAction"] = "";
    const cancel = button("Cancel", () => finish(undefined)); cancel.dataset["hotkey"] = "x";
    footer.append(cancel, confirm);
    control.addEventListener("input", () => { error.hidden = true; error.textContent = ""; control.removeAttribute("aria-invalid"); control.removeAttribute("aria-describedby"); });
    // Enter in a one-line control answers; a text answer may span lines (Ctrl+Enter answers from the dialog).
    if (!(control instanceof HTMLTextAreaElement)) (control as HTMLElement).addEventListener("keydown", (event: KeyboardEvent) => { if (event.key === "Enter") { event.preventDefault(); accept(); } });
    const handle = service.open({ title: options.title, body, size: "sm", footer,
      dismiss: () => { finish(undefined); return true; },
      onClose: () => { if (!settled) { settled = true; resolve(undefined); } },
      ...(options.signal ? { signal: options.signal } : {}) });
  });
}

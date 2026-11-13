/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { editorText, EDITOR_OPTIONS_LIMIT, EDITOR_OPTIONS_BYTES } from "./editor-types.js";
import { el } from "./dom.js";
import { icon } from "./icons.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { ClientError } from "./transport.js";

interface Choice { readonly value: string; readonly label: string }
export interface ChoiceDropdown {
  readonly element: HTMLElement;
  readonly trigger: HTMLButtonElement;
  setValue(value: string): void;
  setDisabled(disabled: boolean): void;
  close(): void;
  dispose(): void;
}

/** Internal value adapter. Keyboard, focus, placement and dismissal belong to the shared menu. */
export function createChoiceDropdown(options: {
  label: string; choices: readonly Choice[]; value: string; onChange(value: string): void;
}): ChoiceDropdown {
  editorText(options.label, "Choice label");
  if (options.choices.length > EDITOR_OPTIONS_LIMIT) throw new ClientError("validation", "Choices exceed 1024 entries or 262144 text bytes.");
  let bytes = 0;
  const values = new Map<string, Choice>();
  for (const choice of options.choices) {
    editorText(choice.value, "Choice identity"); editorText(choice.label, "Choice label");
    bytes += new TextEncoder().encode(choice.value + choice.label).length;
    if (bytes > EDITOR_OPTIONS_BYTES) throw new ClientError("validation", "Choices exceed 1024 entries or 262144 text bytes.");
    if (values.has(choice.value)) throw new ClientError("validation", `Duplicate choice for ${options.label}: ${choice.value}.`);
    values.set(choice.value, { ...choice });
  }
  const validate = (value: string): Choice => {
    const choice = values.get(value);
    if (!choice) throw new ClientError("validation", `Unknown choice for ${options.label}: ${value}.`);
    return choice;
  };
  validate(options.value);
  const element = el("div", undefined, "nav-dropdown choice-dropdown");
  const trigger = el("button", undefined, "choice-dropdown-trigger");
  trigger.type = "button"; trigger.setAttribute("aria-label", options.label);
  const label = el("span"); trigger.append(label, icon("chevron-down"));
  const menu = el("div", undefined, "nav-dropdown-menu choice-dropdown-menu");
  menu.setAttribute("aria-label", options.label);
  let disposed = false, disabled = false;
  const buttons = new Map<string, HTMLButtonElement>();
  const setValue = (value: string): void => {
    const current = validate(value).label;
    label.textContent = current;
    // The name carries the value, as a native select announces it.
    trigger.setAttribute("aria-label", `${options.label}: ${current}`);
    trigger.value = value;
    for (const [key, item] of buttons) item.setAttribute("aria-checked", String(key === value));
  };
  for (const choice of values.values()) {
    const item = el("button", choice.label, "dropdown-item");
    item.type = "button"; item.setAttribute("role", "menuitemradio"); item.dataset["value"] = choice.value;
    item.addEventListener("click", () => {
      if (disposed || disabled) return;
      setValue(choice.value); dropdown.close(true); options.onChange(choice.value);
    });
    buttons.set(choice.value, item); menu.append(item);
  }
  const dropdown = attachMenuDropdown({ trigger, menu });
  // A nested choice owns its keys; do not let an ancestor menu move the same focus twice.
  element.addEventListener("keydown", event => {
    if (["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key) || event.key.length === 1 && event.key !== " " && !event.ctrlKey && !event.metaKey && !event.altKey) event.stopPropagation();
  });
  element.append(trigger, menu); setValue(options.value);
  return {
    element, trigger, setValue, close: () => dropdown.close(false),
    setDisabled(value) {
      disabled = value; trigger.disabled = value;
      for (const item of buttons.values()) item.disabled = value;
      if (value) dropdown.close(false);
    },
    dispose() { if (disposed) return; disposed = true; dropdown.close(false); dropdown.dispose(); }
  };
}

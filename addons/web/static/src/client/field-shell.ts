/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Field } from "./metadata.js";
import { el } from "./dom.js";
/** Tooltip text is derived only from this authorized occurrence; debug adds no field reads. */
export function fieldTooltip(field: Field, widget: string, debug: boolean): string {
  const normal = field.options?.help ?? "";
  if (!debug) return normal;
  return `${normal}${normal ? "\n\n" : ""}${field.label} (${field.name})\nType: ${field.kind}\nWidget: ${widget}${field.relation ? `\nRelation: ${field.relation}` : ""}\nReadonly: ${field.readonly}\nRequired: ${field.required}`;
}
let identity = 0;
/** Accessible state wrapper shared by scalar controls; values remain with the record owner. */
export function fieldShell(field: Field, control: HTMLElement, viewContext: "form" | "list" | "card" = "form"): { node: HTMLElement; invalid(message: string): void; clear(): void } {
  const compact = viewContext === "list";
  const node = el(compact ? "span" : "div", undefined, compact ? "field-shell field-cell" : "field-label field-shell");
  node.dataset["field"] = field.name;
  if (field.kind === "boolean") node.classList.add("field-label-boolean");
  node.classList.toggle("field-required", field.required); node.classList.toggle("field-readonly", field.readonly);
  const id = `field-control-${++identity}`;
  control.id = control.id || id;
  const caption = el("span"); const label = el("label", field.label); label.htmlFor = control.id; caption.append(label);
  if (field.required) { const marker = el("span", " *", "field-required-marker"); marker.setAttribute("aria-hidden", "true"); caption.append(marker); }
  if (!compact && !control.matches("input,textarea,select,button,a[href]")) { if (!control.hasAttribute("role")) control.setAttribute("role", "group"); if (!control.hasAttribute("aria-label")) control.setAttribute("aria-label", field.label); }
  const error = el("span", undefined, "field-error"); error.id = `${id}-error`; error.setAttribute("role", "status"); error.hidden = true;
  if (compact) { node.append(control, error); if (control.matches("input,textarea,select") && !control.hasAttribute("aria-label")) control.setAttribute("aria-label", field.label); }
  else node.append(caption, control, error);
  const help = field.options?.help;
  if (help && compact) node.title = help;
  if (help && !compact) { const detail = el("span", help, "field-help"); detail.id = `${id}-help`; node.append(detail); control.setAttribute("aria-describedby", detail.id); label.title = help; }
  const invalid = (message: string): void => {
    node.classList.add("field-invalid"); control.setAttribute("aria-invalid", "true"); error.textContent = message; error.hidden = false;
    control.setAttribute("aria-describedby", `${help && !compact ? `${id}-help ` : ""}${error.id}`);
    if (control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement || control instanceof HTMLSelectElement) control.setCustomValidity(message);
  };
  return { node, invalid, clear: () => {
    node.classList.remove("field-invalid"); control.removeAttribute("aria-invalid"); error.hidden = true; error.textContent = "";
    if (help && !compact) control.setAttribute("aria-describedby", `${id}-help`); else control.removeAttribute("aria-describedby");
    if (control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement || control instanceof HTMLSelectElement) control.setCustomValidity("");
  } };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { registerFieldWidget, type WidgetEnvironment, type WidgetHandle } from "./registry.js";
import { ClientError } from "./transport.js";

/** Display-only state from authorized selection metadata. Clickability is not admitted by this contract. */
export function statusbarControl({ field, value }: WidgetEnvironment): WidgetHandle {
  if (field.kind !== "selection" || !field.selection) throw new ClientError("server", "The statusbar widget requires selection metadata.");
  const empty = value === false || value === null || value === undefined;
  if (!empty && (typeof value !== "string" || !field.selection.some(([key]) => key === value))) {
    throw new ClientError("server", `Unknown statusbar value for ${field.name}.`);
  }
  const bar = el("div", undefined, "form-statusbar");
  bar.setAttribute("role", "group"); bar.setAttribute("aria-label", field.label);
  const choices = field.selection.filter(([key]) => !field.statusbarVisible || field.statusbarVisible.includes(key) || key === value);
  for (const [key, label] of choices) {
    const step = el("button", label, "statusbar-step"); step.type = "button";
    step.dataset["value"] = key; step.dataset["focusKey"] = `statusbar:${field.name}:${key}`;
    step.disabled = true;
    if (key === value) step.setAttribute("aria-disabled", "true");
    if (key === value) { step.classList.add("is-current"); step.setAttribute("aria-current", "step"); }
    bar.append(step);
  }
  return { node: bar, dispose: () => {} };
}
registerFieldWidget("statusbar", { kinds: ["selection"], render: statusbarControl });

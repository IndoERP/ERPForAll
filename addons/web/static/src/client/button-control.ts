/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon, type IconName } from "./icons.js";
import { ClientError } from "./transport.js";
import type { ButtonVariant, ButtonSize as WireButtonSize } from "../generated/human-ui.js";

export type ButtonRank = ButtonVariant;
export type ButtonSize = WireButtonSize;
/** Client-local presentation. The caller owns execution, confirmation and asynchronous errors. */
export interface ButtonControlOptions {
  readonly label: string;
  readonly action?: () => void;
  readonly rank?: ButtonRank;
  readonly size?: ButtonSize;
  readonly icon?: IconName;
  readonly accessibleLabel?: string;
  readonly title?: string;
  readonly circle?: boolean;
  readonly disabled?: boolean;
  readonly pending?: boolean;
  readonly className?: string;
}
const ranks: readonly string[] = ["primary", "secondary", "link", "success", "info", "warning", "danger", "unstyled"];
const sizes: readonly string[] = ["small", "normal", "large"];
function boundedText(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string") throw new ClientError("validation", `Button ${label} must be text.`);
  if (value.length > 4096) throw new ClientError("validation", `Button ${label} exceeds the 4096-character limit.`);
}
/** One native action target; presentation never chooses a command or adds confirmation. */
export function createButtonControl(options: ButtonControlOptions): HTMLButtonElement {
  boundedText(options.label, "label");
  for (const key of ["accessibleLabel", "title", "className"] as const) {
    if (options[key] !== undefined) boundedText(options[key], key);
  }
  for (const key of ["circle", "disabled", "pending"] as const) {
    if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ClientError("validation", `Button ${key} must be a boolean.`);
  }
  if (options.action !== undefined && typeof options.action !== "function") throw new ClientError("validation", "Button action must be a function.");
  const rank = options.rank === undefined ? "secondary" : options.rank, size = options.size === undefined ? "normal" : options.size;
  if (!ranks.includes(rank)) throw new ClientError("validation", `Unknown button rank: ${rank}.`);
  if (!sizes.includes(size)) throw new ClientError("validation", `Unknown button size: ${size}.`);
  const name = options.accessibleLabel?.trim() || options.label.trim() || options.title?.trim();
  if (!name) throw new ClientError("validation", "A button requires a visible label, accessible label or title.");
  const control = document.createElement("button");
  control.type = "button";
  control.className = `erp-button erp-button-${rank} erp-button-${size}`;
  if (options.className) control.className += ` ${options.className}`;
  if (options.circle) control.classList.add("erp-button-circle");
  if (options.icon !== undefined) control.append(icon(options.icon));
  if (options.label) control.append(document.createTextNode(options.label));
  if (options.accessibleLabel?.trim() || !options.label.trim()) control.setAttribute("aria-label", name);
  if (options.title !== undefined) control.title = options.title;
  control.disabled = options.disabled === true || options.pending === true || options.action === undefined;
  if (options.pending) { control.setAttribute("aria-busy", "true"); control.classList.add("is-pending"); }
  const action = options.action;
  if (action) control.addEventListener("click", () => { if (!control.disabled) action(); });
  return control;
}

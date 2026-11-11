/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { editorOptions } from "./editor-types.js";
import { ClientError } from "./transport.js";
export interface ColorPaletteOptions { value?: number; excludeNone?: boolean; disabled?: boolean; select(index: number): void }
const names = ["No colour", "Red", "Orange", "Yellow", "Cyan", "Purple", "Almond", "Teal", "Blue", "Raspberry", "Green", "Violet"] as const;
/** Fixed semantic indices; selection does not parse arbitrary CSS. */
export function createColorPalette(options: ColorPaletteOptions): { node: HTMLElement; dispose(): void } {
  editorOptions(options, ["value", "excludeNone", "disabled", "select"], "Colour palette");
  if (options.value !== undefined && (!Number.isInteger(options.value) || options.value < 0 || options.value >= names.length)) throw new ClientError("validation", "Colour index must be an integer from 0 to 11.");
  if (typeof options.select !== "function" || options.excludeNone !== undefined && typeof options.excludeNone !== "boolean" || options.disabled !== undefined && typeof options.disabled !== "boolean") throw new ClientError("validation", "Invalid colour palette declaration.");
  const node = el("div", undefined, "choice-palette"); node.setAttribute("role", "group"); node.setAttribute("aria-label", "Colour palette");
  let dead = false;
  names.forEach((name, index) => {
    if (index === 0 && options.excludeNone) return;
    const control = button("", () => { if (!dead) options.select(index); }, "choice-swatch");
    control.setAttribute("aria-label", name); control.title = name; control.setAttribute("aria-pressed", String(options.value === index));
    control.dataset["colorIndex"] = String(index); control.disabled = options.disabled === true;
    node.append(control);
  });
  return { node, dispose: () => { dead = true; } };
}

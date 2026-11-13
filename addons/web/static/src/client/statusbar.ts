/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { createButtonMenu } from "./button-menu.js";
import { icon } from "./icons.js";
import { registerFieldWidget, type WidgetEnvironment, type WidgetHandle } from "./registry.js";
import { ClientError } from "./transport.js";

type Choice = readonly [string, string];

/**
 * Header state track for a selection field. A list of steps, not a radio group: moving the arrow keys only moves
 * focus, because choosing a state is a business transition that must be a deliberate Enter, Space or click
 * (a radio group would select on focus). The current step carries `aria-current="step"`.
 *
 * Display-only unless the occurrence is `clickable` and the field is writable; a display-only track draws plain
 * text steps (no buttons, no pointer). States listed as `folded` sit behind a "More states" menu at the start of
 * the track. Below 700px the track is replaced by one control showing the current state (CSS decides which one is
 * shown; both exist in the DOM, the hidden one is out of the accessibility tree).
 */
export function statusbarControl({ field, value, change }: WidgetEnvironment): WidgetHandle {
  if (field.kind !== "selection" || !field.selection) throw new ClientError("server", "The statusbar widget requires selection metadata.");
  const empty = value === false || value === null || value === undefined;
  if (!empty && (typeof value !== "string" || !field.selection.some(([key]) => key === value))) {
    throw new ClientError("server", `Unknown statusbar value for ${field.name}.`);
  }
  const clickable = field.statusbarClickable === true && !field.readonly && typeof change === "function";
  const choices: Choice[] = field.selection.filter(([key]) => !field.statusbarVisible || field.statusbarVisible.includes(key) || key === value);
  const foldedKeys = new Set(field.statusbarFolded ?? []);
  const currentIndex = choices.findIndex(([key]) => key === value);
  const inline = choices.filter(([key]) => !foldedKeys.has(key) || key === value);
  const folded = choices.filter(([key]) => foldedKeys.has(key) && key !== value);
  const pick = (key: string): void => { if (key !== value) change(key); };
  const disposers: (() => void)[] = [];

  const bar = el("div", undefined, "form-statusbar");
  bar.setAttribute("role", "group"); bar.setAttribute("aria-label", field.label);
  bar.dataset["clickable"] = String(clickable);
  const track = el("ol", undefined, "statusbar-track");
  const stops: HTMLButtonElement[] = [];

  const menu = (opener: HTMLButtonElement, host: HTMLElement, entries: readonly Choice[]): void => {
    const list = el("div", undefined, "statusbar-menu");
    const items = entries.map(([key, label]) => {
      const item = el("button", label, "statusbar-menu-item"); item.type = "button"; item.dataset["state"] = key;
      if (key === value) { item.disabled = true; item.setAttribute("aria-current", "true"); } else item.addEventListener("click", () => pick(key));
      return item;
    });
    list.append(...items); host.append(opener, list);
    const controller = createButtonMenu({ opener, menu: list, items: () => items.filter(item => !item.disabled) });
    disposers.push(controller.dispose);
  };

  if (clickable && folded.length) {
    const item = el("li", undefined, "statusbar-item");
    const more = el("button", "…", "statusbar-step statusbar-more"); more.type = "button";
    more.setAttribute("aria-label", "More states"); more.dataset["focusKey"] = `statusbar:${field.name}:more`;
    menu(more, item, folded); stops.push(more); track.append(item);
  }
  for (const [key, label] of inline) {
    const index = choices.findIndex(([candidate]) => candidate === key);
    const live = clickable && key !== value;
    const item = el("li", undefined, "statusbar-item");
    const step = live ? el("button", label, "statusbar-step") : el("span", label, "statusbar-step");
    step.dataset["value"] = key;
    if (step instanceof HTMLButtonElement) {
      step.type = "button"; step.dataset["focusKey"] = `statusbar:${field.name}:${key}`;
      step.addEventListener("click", () => pick(key)); stops.push(step);
    }
    if (key === value) { step.classList.add("is-current"); step.setAttribute("aria-current", "step"); }
    else if (currentIndex >= 0 && index < currentIndex) step.classList.add("is-passed");
    item.append(step); track.append(item);
  }

  // One tab stop for the track; arrows, Home and End move focus between the clickable steps only.
  const roving = (focused?: HTMLElement): void => { for (const stop of stops) stop.tabIndex = stop === (focused ?? stops[0]) ? 0 : -1; };
  roving();
  track.addEventListener("focusin", event => { const target = event.target; if (target instanceof HTMLButtonElement && stops.includes(target)) roving(target); });
  track.addEventListener("keydown", event => {
    const target = event.target;
    if (!(target instanceof HTMLButtonElement) || !stops.includes(target) || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const rtl = bar.ownerDocument.defaultView?.getComputedStyle(bar).direction === "rtl";
    const forward = rtl ? "ArrowLeft" : "ArrowRight", backward = rtl ? "ArrowRight" : "ArrowLeft";
    const at = stops.indexOf(target);
    const next = event.key === forward ? Math.min(at + 1, stops.length - 1) : event.key === backward ? Math.max(at - 1, 0) : event.key === "Home" ? 0 : event.key === "End" ? stops.length - 1 : -1;
    if (next < 0) return;
    event.preventDefault(); stops[next]!.focus();
  });
  bar.append(track);

  // Narrow widths: the current state as one control; it opens the full list when the field is writable.
  const currentLabel = choices.find(([key]) => key === value)?.[1] ?? field.label;
  const compact = el("div", undefined, "statusbar-compact");
  if (clickable) {
    const opener = el("button", currentLabel, "statusbar-compact-trigger"); opener.type = "button";
    opener.setAttribute("aria-label", `${field.label}: ${currentLabel}`); opener.append(icon("chevron-down"));
    menu(opener, compact, choices);
  } else compact.append(el("span", currentLabel, "statusbar-compact-value"));
  bar.append(compact);

  return { node: bar, dispose: () => { for (const dispose of disposers.splice(0)) dispose(); } };
}
registerFieldWidget("statusbar", { kinds: ["selection"], render: statusbarControl });

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Field } from "./metadata.js";
import { ClientError } from "./transport.js";
export interface SelectionCounts { total: number; values: Map<string, number>; unset: number }
function invalid(message: string): never { throw new ClientError("server", message); }

/** Validate the bounded opt-in contract independently of visible field names. */
export function validateSelectionTabField(field: Field): void {
  if (field.kind !== "selection" || !field.selection || field.selection.length < 1 || field.selection.length > 32) invalid("List tabs require 1–32 declared selection choices.");
  const keys = new Set<string>(), encoder = new TextEncoder();
  for (const [key, label] of field.selection) {
    if (!key.trim() || !label.trim() || encoder.encode(key).length > 256 || encoder.encode(label).length > 256 || keys.has(key)) invalid("List tab choices require distinct nonempty keys and nonempty labels of at most 256 bytes.");
    keys.add(key);
  }
}

/** Decode one authorized grouped read; omitted declared groups mean zero, malformed reads never do. */
export function decodeSelectionCounts(raw: unknown, field: Field): SelectionCounts {
  validateSelectionTabField(field);
  if (!Array.isArray(raw) || raw.length > field.selection!.length + 1) invalid("Invalid selection tab groups.");
  const values = new Map(field.selection!.map(([key]) => [key, 0])), seen = new Set<string | false>();
  let total = 0, unset = 0;
  for (const item of raw) {
    if (item === null || typeof item !== "object" || Array.isArray(item)) invalid("Invalid selection tab group.");
    const row = item as Record<string, unknown>;
    if (Object.keys(row).some(key => key !== field.name && key !== "__count" && key !== "__extra_domain")) invalid("Unknown selection tab count property.");
    const key = row[field.name], count = row["__count"];
    if (typeof key !== "string" && key !== false) invalid("Invalid selection tab group value.");
    if (key === false && field.required) invalid("Required selection tab field returned an unset group.");
    if (typeof key === "string" && !values.has(key)) invalid(`Unknown selection tab group: ${key}.`);
    if (seen.has(key)) invalid("Duplicate selection tab group.");
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) invalid("Invalid selection tab count.");
    if (!Number.isSafeInteger(total + count)) invalid("Selection tab total exceeds the browser safe integer range.");
    seen.add(key); total += count;
    if (key === false) unset = count;
    else values.set(key, count);
  }
  return { total, values, unset };
}

/** Native buttons provide real filtering, bounded counts, roving focus and URL-safe explicit tokens. */
export function createListTabs(config: { field: Field; panel?: HTMLElement; initial: string | undefined; label: string; enableCounters: boolean; expand: boolean; change: (token: string | undefined) => boolean | void }): {
  element: HTMLElement; rowDomain(base: unknown[]): unknown[]; publish(counts: SelectionCounts): void; reconcile(counts: SelectionCounts): boolean; finish(): void; invalidate(): void;
} {
  const { field, panel, initial, change, label: categoryLabel, enableCounters, expand } = config;
  validateSelectionTabField(field);
  const choices: { token: string | undefined; label: string; value: string | undefined }[] = [
    { token: undefined, label: "All", value: undefined },
    ...field.selection!.map(([value, label]) => ({ token: `value:${value}`, label, value }))
  ];
  if (!choices.some(choice => choice.token === initial)) throw new ClientError("validation", "Invalid list tab selection.");
  let selected = initial;
  const element = document.createElement("div");
  element.className = "list-state-tabs";
  element.setAttribute("role", "tablist"); element.setAttribute("aria-label", `${categoryLabel} filters`);
  element.setAttribute("aria-busy", "true");
  const buttons: HTMLButtonElement[] = [], counters: HTMLElement[] = [];
  const labelPanel = (index: number): void => {
    if (panel) panel.setAttribute("aria-labelledby", buttons[index]!.id);
  };
  if (panel) { panel.setAttribute("role", "tabpanel"); panel.tabIndex = 0; }
  const select = (index: number, focus: boolean): void => {
    const choice = choices[index]!, previous = selected;
    const changed = selected !== choice.token; selected = choice.token;
    if (changed && change(selected) === false) { selected = previous; return; }
    buttons.forEach((button, at) => { button.tabIndex = at === index ? 0 : -1; button.setAttribute("aria-selected", String(at === index)); });
    labelPanel(index);
    if (focus) buttons[index]!.focus();
  };
  choices.forEach((choice, index) => {
    const button = document.createElement("button"), label = document.createElement("span"), count = document.createElement("span");
    button.type = "button"; button.className = "list-state-tab"; button.setAttribute("role", "tab"); button.title = choice.label;
    if (panel) {
      button.id = `${panel.id}-category-${index}`;
      button.setAttribute("aria-controls", panel.id);
    }
    button.dataset["tabToken"] = choice.token ?? "all";
    label.textContent = choice.label; count.textContent = "—"; count.className = "list-tab-count"; count.hidden = !enableCounters;
    button.append(label, count); button.addEventListener("click", () => select(index, false));
    button.addEventListener("keydown", event => {
      const visible = buttons.flatMap((button, at) => button.hidden ? [] : [at]), position = visible.indexOf(index);
      const target = event.key === "ArrowRight" ? visible[(position + 1) % visible.length] : event.key === "ArrowLeft" ? visible[(position + visible.length - 1) % visible.length] : event.key === "Home" ? visible[0] : event.key === "End" ? visible.at(-1) : undefined;
      if (target !== undefined) { event.preventDefault(); select(target, true); }
    });
    buttons.push(button); counters.push(count); element.append(button);
  });
  select(choices.findIndex(choice => choice.token === initial), false);
  return {
    element,
    rowDomain(base) {
      const choice = choices.find(item => item.token === selected)!;
      return choice.value === undefined ? [...base] : [...base, [field.name, "=", choice.value]];
    },
    reconcile(counts) {
      choices.forEach((choice, index) => { buttons[index]!.hidden = !expand && choice.value !== undefined && counts.values.get(choice.value) === 0; });
      const current = choices.findIndex(choice => choice.token === selected);
      if (!buttons[current]!.hidden) return false;
      const restoreFocus = element.contains(document.activeElement);
      selected = undefined;
      buttons.forEach((button, index) => { button.tabIndex = index === 0 ? 0 : -1; button.setAttribute("aria-selected", String(index === 0)); });
      labelPanel(0);
      if (restoreFocus) buttons[0]!.focus();
      return true;
    },
    finish() { element.setAttribute("aria-busy", "false"); },
    invalidate() {
      counters.forEach((counter, index) => { counter.textContent = "—"; buttons[index]!.setAttribute("aria-label", enableCounters ? `${choices[index]!.label}, count unavailable` : choices[index]!.label); });
      element.setAttribute("aria-busy", "false");
    },
    publish(counts) {
      choices.forEach((choice, index) => {
        const count = choice.value === undefined ? counts.total : counts.values.get(choice.value)!;
        counters[index]!.textContent = String(count);
        buttons[index]!.setAttribute("aria-label", enableCounters ? `${choice.label}, ${count} records` : choice.label);
      });
      element.setAttribute("aria-busy", "false");
    }
  };
}

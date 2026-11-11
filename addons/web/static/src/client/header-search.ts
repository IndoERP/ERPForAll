/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { icon } from "./icons.js";

export interface MenuSearchEntry { id: number; label: string; action: number; menu: number }
let nextSearch = 0;
/** A search of already-authorized menus, never a substitute for a record-search service. */
export function headerMenuSearch(env: { entries(): readonly MenuSearchEntry[]; choose(entry: MenuSearchEntry): void }): { node: HTMLElement; ready(): void; dispose(): void } {
  const node = el("div", undefined, "office-header-search"), input = el("input"), results = el("div", undefined, "office-menu-results");
  input.type = "search"; input.maxLength = 160; input.placeholder = "Search apps and menus…"; input.disabled = true;
  input.setAttribute("aria-label", "Search apps and menus"); input.setAttribute("role", "combobox");
  input.setAttribute("aria-autocomplete", "list"); input.setAttribute("aria-expanded", "false");
  results.id = `header-menu-results-${++nextSearch}`; results.setAttribute("role", "listbox"); results.hidden = true;
  input.setAttribute("aria-controls", results.id); node.append(icon("search"), input, results);
  let shown: readonly MenuSearchEntry[] = [], active = -1, disposed = false;
  const close = (): void => { results.hidden = true; input.setAttribute("aria-expanded", "false"); input.removeAttribute("aria-activedescendant"); active = -1; };
  const select = (entry: MenuSearchEntry): void => { input.value = ""; input.focus(); close(); env.choose(entry); };
  const highlight = (): void => {
    [...results.querySelectorAll<HTMLElement>('[role="option"]')].forEach((option, index) => option.setAttribute("aria-selected", String(index === active)));
    const option = results.children[active];
    if (option instanceof HTMLElement) { input.setAttribute("aria-activedescendant", option.id); option.scrollIntoView?.({ block: "nearest" }); }
    else input.removeAttribute("aria-activedescendant");
  };
  const render = (): void => {
    if (input.disabled) return;
    const query = input.value.trim().normalize("NFKC").toLowerCase();
    shown = env.entries().filter(entry => entry.label.normalize("NFKC").toLowerCase().includes(query)).slice(0, 20);
    active = -1; results.replaceChildren();
    for (const [index, entry] of shown.entries()) {
      // Data labels never choose command icons or behavior.
      const option = button(entry.label, () => select(entry), "office-menu-option");
      option.id = `${results.id}-${index}`; option.setAttribute("role", "option"); option.setAttribute("aria-selected", "false"); option.tabIndex = -1;
      results.append(option);
    }
    if (!shown.length) results.append(el("p", "No matching apps or menus.", "office-menu-empty"));
    results.hidden = false; input.setAttribute("aria-expanded", "true"); input.removeAttribute("aria-activedescendant");
  };
  input.addEventListener("input", render); input.addEventListener("focus", render);
  input.addEventListener("blur", event => { if (!(event.relatedTarget instanceof Node) || !node.contains(event.relatedTarget)) close(); });
  input.addEventListener("keydown", event => {
    if (input.disabled) return;
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(); return; }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault(); if (results.hidden) render();
      if (shown.length) { active = active < 0 ? (event.key === "ArrowDown" ? 0 : shown.length - 1) : (active + (event.key === "ArrowDown" ? 1 : -1) + shown.length) % shown.length; highlight(); }
    } else if (event.key === "Enter" && !results.hidden) {
      const entry = shown[active] ?? (shown.length === 1 ? shown[0] : undefined);
      if (entry) { event.preventDefault(); select(entry); }
    } else if (event.key === "Tab") close();
  });
  const outside = (event: Event): void => { if (event.target instanceof Node && !node.contains(event.target)) close(); };
  document.addEventListener("pointerdown", outside);
  return { node, ready(): void { if (!disposed) input.disabled = false; }, dispose(): void { disposed = true; input.disabled = true; document.removeEventListener("pointerdown", outside); close(); results.replaceChildren(); } };
}

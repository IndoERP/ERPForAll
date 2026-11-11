/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { DEFAULT_MENU_ICON, icon, knownMenuIcon, menuIcon, menuIconChoices, menuIconValue } from "./icons.js";
import { registerFieldWidget, type WidgetEnvironment, type WidgetHandle } from "./registry.js";

let pickerCount = 0;

/** `web.menu_icon`: picks an app menu's icon from the icons this client ships. The field keeps its
 *  `lucide:<name>` text value; an empty value means the default icon.
 *
 *  The field is a labelled group of native buttons (no listbox semantics): each icon choice is a
 *  toggle button with `aria-pressed`. A choice and "Use default" hand focus back to "Choose icon"
 *  once the form has re-rendered (`data-focus-return`, honoured by the form). Escape in the search
 *  box closes the panel and returns focus to the toggle. */
export function menuIconPicker(environment: WidgetEnvironment): WidgetHandle {
  const { field, value, change } = environment;
  const current = typeof value === "string" && value !== "" ? value : null;
  const known = knownMenuIcon(current);
  const id = `menu-icon-${++pickerCount}`;
  const toggleKey = `menu-icon:${field.name}:toggle`;

  const wrapper = el("div", undefined, "field-label menu-icon-field");
  wrapper.setAttribute("role", "group");
  const label = el("span", field.label + (field.required ? " *" : ""));
  label.id = `${id}-label`;
  wrapper.setAttribute("aria-labelledby", label.id);
  if (field.required) wrapper.classList.add("field-required");
  if (field.readonly) wrapper.classList.add("field-readonly");

  const area = el("div", undefined, "menu-icon-picker");
  const preview = el("span", undefined, "menu-icon-preview");
  preview.append(menuIcon(current));
  const caption = el("span", known ?? (current === null ? `Default (${DEFAULT_MENU_ICON})` : current), "menu-icon-name");
  const head = el("div", undefined, "menu-icon-head");
  head.append(preview, caption);
  area.append(head);
  if (current !== null && known === null) {
    area.append(el("p", "This icon is not shipped with this version; the default icon is shown.", "menu-icon-note"));
  }
  if (!field.readonly) {
    const panel = el("div", undefined, "menu-icon-panel");
    panel.id = `${id}-panel`;
    panel.hidden = true;

    const toggle = el("button", "Choose icon", "menu-icon-toggle");
    toggle.type = "button";
    toggle.dataset["focusKey"] = toggleKey;
    toggle.setAttribute("aria-expanded", "false");
    toggle.setAttribute("aria-controls", panel.id);
    const reset = el("button", "Use default", "menu-icon-reset");
    reset.type = "button";
    reset.dataset["focusReturn"] = toggleKey;
    reset.disabled = current === null;
    reset.addEventListener("click", () => change(false));
    head.append(toggle, reset);

    const search = el("input", undefined, "menu-icon-search");
    search.type = "search";
    search.placeholder = "Search icons";
    search.setAttribute("aria-label", "Search icons");
    const grid = el("div", undefined, "menu-icon-grid");
    grid.setAttribute("role", "group");
    grid.setAttribute("aria-label", `${field.label} choices`);
    const choices = menuIconChoices().map(name => {
      const choice = el("button", undefined, "menu-icon-choice");
      choice.type = "button";
      choice.title = name;
      choice.setAttribute("aria-label", name);
      choice.setAttribute("aria-pressed", String(name === known));
      choice.dataset["iconChoice"] = name;
      choice.dataset["focusReturn"] = toggleKey;
      choice.append(icon(name));
      choice.addEventListener("click", () => change(menuIconValue(name)));
      grid.append(choice);
      return { name, choice };
    });
    const empty = el("p", "No icon matches.", "menu-icon-note");
    empty.hidden = true;
    search.addEventListener("input", () => {
      const query = search.value.trim().toLowerCase();
      let shown = 0;
      for (const { name, choice } of choices) {
        choice.hidden = query !== "" && !name.includes(query);
        if (!choice.hidden) shown++;
      }
      empty.hidden = shown !== 0;
    });
    const setOpen = (open: boolean): void => {
      panel.hidden = !open;
      toggle.setAttribute("aria-expanded", String(open));
    };
    panel.addEventListener("keydown", event => {
      if (event.key !== "Escape" || panel.hidden) return;
      event.preventDefault();
      event.stopPropagation();
      setOpen(false);
      toggle.focus();
    });
    panel.append(search, grid, empty);
    toggle.addEventListener("click", () => {
      const open = panel.hidden !== false;
      setOpen(open);
      if (open) search.focus();
    });
    area.append(panel);
  }
  wrapper.append(label, area);
  return { node: wrapper, dispose: () => {} };
}

registerFieldWidget("web.menu_icon", { kinds: ["char"], render: menuIconPicker });

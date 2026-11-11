/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import { ClientError } from "./transport.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

const COLOR_PALETTE = [
  { index: 0, name: "No color" },
  { index: 1, name: "Red" },
  { index: 2, name: "Orange" },
  { index: 3, name: "Yellow" },
  { index: 4, name: "Cyan" },
  { index: 5, name: "Purple" },
  { index: 6, name: "Almond" },
  { index: 7, name: "Teal" },
  { index: 8, name: "Blue" },
  { index: 9, name: "Raspberry" },
  { index: 10, name: "Green" },
  { index: 11, name: "Violet" },
] as const;

const HEX_COLOR_REGEX = /^#(?:[0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

function normalizeHex(hex: string): string {
  if (hex.length === 4) {
    return `#${hex[1]}${hex[1]}${hex[2]}${hex[2]}${hex[3]}${hex[3]}`.toLowerCase();
  }
  return hex.toLowerCase();
}

/** `color` widget renders a char field as a hex color swatch in read mode and a native color input in edit mode. */
export function colorControl(options: FieldControlOptions): FieldControl {
  const { field, value, change } = options;
  if (field.kind !== "char") {
    throw new ClientError("server", `Field ${field.name} with color widget must be a char field.`);
  }

  let hexValue = "";
  if (value !== null && value !== undefined && value !== false && value !== "") {
    if (typeof value !== "string" || !HEX_COLOR_REGEX.test(value.trim())) {
      throw new ClientError("validation", `Field ${field.name} contains invalid color hex: '${String(value)}'.`);
    }
    hexValue = normalizeHex(value.trim());
  }

  const container = el("div", undefined, "field-color");
  const shell = fieldShell(field, container, options.viewContext);

  if (field.required && !hexValue) {
    shell.invalid(`${field.label} is required.`);
  }

  const labelName = hexValue ? hexValue : "No color";

  if (field.readonly) {
    const swatch = el("span", undefined, "field-color-swatch");
    swatch.setAttribute("role", "img");
    swatch.setAttribute("aria-label", `${field.label ? field.label + ": " : ""}${labelName}`);
    swatch.title = labelName;
    if (hexValue) {
      swatch.style.backgroundColor = hexValue;
    } else {
      swatch.classList.add("field-color-empty");
    }
    container.classList.add("field-color-readonly");
    container.append(swatch);
    return { node: shell.node, dispose: () => {} };
  }

  // Edit mode: a visible swatch (hatched when empty) with the native color input laid over it,
  // plus a clear control so an empty value can be restored.
  container.classList.add("field-color-edit");
  const editSwatch = el("span", undefined, "field-color-swatch");
  editSwatch.setAttribute("aria-hidden", "true");
  const input = el("input", undefined, "field-color-input");
  input.type = "color";
  input.name = field.name;
  input.value = hexValue || "#000000";
  input.setAttribute("aria-label", field.label);
  const clearButton = el("button", "\u00d7", "field-color-clear");
  clearButton.type = "button";
  clearButton.setAttribute("aria-label", `Clear ${field.label}`);
  clearButton.title = `Clear ${field.label}`;

  let current = hexValue;
  const paint = (hex: string) => {
    editSwatch.style.backgroundColor = hex;
    editSwatch.classList.toggle("field-color-empty", !hex);
    clearButton.hidden = !hex;
    const name = hex || "No color";
    input.title = name;
    input.setAttribute("aria-description", name);
  };
  paint(current);

  input.addEventListener("input", () => paint(normalizeHex(input.value.trim())));
  input.addEventListener("change", () => {
    // A native color input always yields a valid #rrggbb value.
    current = normalizeHex(input.value.trim());
    shell.clear();
    paint(current);
    change(current);
  });
  clearButton.addEventListener("click", () => {
    current = "";
    input.value = "#000000";
    paint("");
    if (field.required) shell.invalid(`${field.label} is required.`);
    else shell.clear();
    change("");
    input.focus();
  });

  container.append(editSwatch, input, clearButton);

  return { node: shell.node, dispose: () => {} };
}

/** `color_picker` widget renders an integer palette index with a color swatch in read mode and a 12-color palette dropdown in edit mode. */
export function colorPickerControl(options: FieldControlOptions): FieldControl {
  const { field, value, change } = options;
  if (field.kind !== "integer") {
    throw new ClientError("server", `Field ${field.name} with color_picker widget must be an integer field.`);
  }

  let selectedIndex = 0;
  if (value !== null && value !== undefined && value !== false && value !== "") {
    const num = Number(value);
    if (!Number.isInteger(num) || num < 0 || num > 11) {
      throw new ClientError("validation", `Field ${field.name} with color_picker widget requires integer between 0 and 11, got '${String(value)}'.`);
    }
    selectedIndex = num;
  }

  const container = el("div", undefined, "field-color-picker");
  const shell = fieldShell(field, container, options.viewContext);

  const currentItem = COLOR_PALETTE[selectedIndex]!;

  if (field.readonly) {
    const swatch = el("span", undefined, `field-color-toggler field-color-item-${selectedIndex}`);
    swatch.setAttribute("role", "img");
    swatch.setAttribute("aria-label", `${field.label ? field.label + ": " : ""}${currentItem.name}`);
    swatch.title = currentItem.name;
    container.append(swatch);
    return { node: shell.node, dispose: () => {} };
  }

  // Edit mode: toggle button + palette panel
  const toggler = el("button", undefined, `field-color-toggler field-color-item-${selectedIndex}`);
  toggler.type = "button";
  toggler.setAttribute("aria-haspopup", "true");
  toggler.setAttribute("aria-expanded", "false");
  toggler.setAttribute("aria-label", `${field.label}: ${currentItem.name}`);
  toggler.title = currentItem.name;

  const panel = el("div", undefined, "field-color-palette");
  panel.setAttribute("role", "menu");
  panel.setAttribute("aria-label", `${field.label} palette`);
  panel.hidden = true;

  const buttons: HTMLButtonElement[] = [];

  const setOpen = (open: boolean) => {
    panel.hidden = !open;
    toggler.setAttribute("aria-expanded", String(open));
    if (open) {
      const activeBtn = buttons[selectedIndex] ?? buttons[0];
      activeBtn?.focus();
    }
  };
  const closeToToggler = () => {
    setOpen(false);
    toggler.focus();
  };

  const selectColor = (idx: number) => {
    // Propose the change first: if it throws, neither the UI nor the panel state has moved.
    change(idx);
    shell.clear();
    selectedIndex = idx;
    const selectedItem = COLOR_PALETTE[idx]!;
    toggler.className = `field-color-toggler field-color-item-${idx}`;
    toggler.setAttribute("aria-label", `${field.label}: ${selectedItem.name}`);
    toggler.title = selectedItem.name;
    for (let i = 0; i < buttons.length; i++) {
      const btn = buttons[i]!;
      btn.classList.toggle("active", i === idx);
      btn.tabIndex = i === idx ? 0 : -1;
    }
    closeToToggler();
  };

  // No stopPropagation: the document-level outside-click handlers of other open
  // dropdowns (header menus, other widgets) must still see clicks on this widget.
  toggler.addEventListener("click", () => {
    setOpen(Boolean(panel.hidden));
  });

  const focusAt = (i: number) => buttons[(i + buttons.length) % buttons.length]?.focus();

  for (let i = 0; i < COLOR_PALETTE.length; i++) {
    const item = COLOR_PALETTE[i]!;
    const btn = el("button", undefined, `field-color-item field-color-item-${item.index}${item.index === selectedIndex ? " active" : ""}`);
    btn.type = "button";
    btn.setAttribute("role", "menuitem");
    btn.setAttribute("aria-label", item.name);
    btn.title = item.name;
    btn.dataset["color"] = String(item.index);
    btn.tabIndex = item.index === selectedIndex ? 0 : -1;

    btn.addEventListener("click", () => selectColor(item.index));

    btn.addEventListener("keydown", (e: KeyboardEvent) => {
      if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        e.preventDefault();
        focusAt(i + 1);
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        e.preventDefault();
        focusAt(i - 1);
      } else if (e.key === "Home") {
        e.preventDefault();
        focusAt(0);
      } else if (e.key === "End") {
        e.preventDefault();
        focusAt(buttons.length - 1);
      } else if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        closeToToggler();
      } else if (e.key === "Tab") {
        // Close and let the default Tab move on from the toggler.
        closeToToggler();
      } else if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        selectColor(item.index);
      }
    });

    buttons.push(btn);
    panel.append(btn);
  }

  // Focus leaving the widget (pointer or programmatic) closes the panel without stealing focus.
  container.addEventListener("focusout", (e: FocusEvent) => {
    const next = e.relatedTarget as Node | null;
    if (!panel.hidden && next && !container.contains(next)) setOpen(false);
  });

  const onDocClick = (e: MouseEvent) => {
    if (!panel.hidden && !container.contains(e.target as Node)) {
      setOpen(false);
    }
  };
  document.addEventListener("click", onDocClick);

  container.append(toggler, panel);

  return {
    node: shell.node,
    dispose: () => {
      document.removeEventListener("click", onDocClick);
    },
  };
}

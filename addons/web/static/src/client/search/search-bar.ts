/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon } from "../icons.js";
import type { AutocompleteItem } from "./types.js";
import type { SearchModel } from "./search-model.js";
import { createSearchMenuPanel } from "./search-menu-panel.js";

export function createSearchBar(
  model: SearchModel,
  onSearchChange: () => void | Promise<void>
): {
  element: HTMLElement;
  input: HTMLInputElement;
  form: HTMLFormElement;
  fieldSelect: HTMLSelectElement;
  render(): void;
  dispose(): void;
} {
  const form = document.createElement("form");
  form.className = "search-form erp-search-view";
  form.setAttribute("role", "search");

  const errorNotice = document.createElement("p");
  errorNotice.setAttribute("role", "alert");
  errorNotice.hidden = true;
  const showError = (error: unknown): void => {
    errorNotice.hidden = false;
    errorNotice.textContent = error instanceof Error ? error.message : "Unable to update this search.";
  };
  const notifySearchChange = (): void => {
    errorNotice.hidden = true;
    try { Promise.resolve(onSearchChange()).catch(showError); }
    catch (error) { showError(error); }
  };

  const inputContainer = document.createElement("div");
  inputContainer.className = "erp-search-input-container";

  // Search magnifying glass icon
  const iconWrapper = document.createElement("span");
  iconWrapper.className = "erp-search-icon-wrapper";
  iconWrapper.append(icon("search"));

  const chipsContainer = document.createElement("div");
  chipsContainer.className = "search-chips erp-search-chips";
  chipsContainer.setAttribute("role", "list");
  chipsContainer.setAttribute("aria-label", "Search filters");

  const input = document.createElement("input");
  input.type = "search";
  input.className = "search-input o_searchview_input";
  input.placeholder = "Search…";
  input.setAttribute("role", "searchbox");
  input.setAttribute("aria-label", "Search records");
  input.autocomplete = "off";
  input.maxLength = 256;
  let editingFacetId: string | undefined;

  // Search field select (accessible, styled, and backward-compatible)
  const fieldSelect = document.createElement("select");
  fieldSelect.className = "search-field-select";
  fieldSelect.setAttribute("aria-label", "Search field");
  for (const field of model.getSearchableFields()) {
    fieldSelect.append(new Option(field.label || field.name, field.name));
  }
  if (model.fields.has("name")) {
    fieldSelect.value = "name";
  }

  inputContainer.append(iconWrapper, chipsContainer, input);

  // Clear button (shown when filters exist or user wants quick reset)
  const clearBtn = document.createElement("button");
  clearBtn.type = "button";
  clearBtn.className = "search-clear-btn";
  clearBtn.textContent = "Clear";
  clearBtn.setAttribute("aria-label", "Clear search filters");
  clearBtn.style.display = "none";
  clearBtn.addEventListener("click", () => {
    input.value = "";
    editingFacetId = undefined;
    model.clearAll();
    closeAutocomplete();
    renderFacets();
    notifySearchChange();
  });

  // Search Menu Toggle Button (Filters / Group By / Favorites)
  const menuToggle = document.createElement("button");
  menuToggle.type = "button";
  menuToggle.className = "search-menu-toggle o_searchview_dropdown_toggler";
  menuToggle.setAttribute("aria-label", "Toggle Search Filters, Group By, and Favorites");
  menuToggle.title = "Filters, Group By, Favorites";
  menuToggle.append(icon("chevron-down"));

  // Hidden submit button for native form submission
  const submitBtn = document.createElement("button");
  submitBtn.type = "submit";
  submitBtn.className = "search-submit-btn";
  submitBtn.style.display = "none";
  submitBtn.textContent = "Search";

  form.append(fieldSelect, inputContainer, clearBtn, menuToggle, submitBtn, errorNotice);

  // Search Menu Panel
  const menuPanel = createSearchMenuPanel(model, () => {
    renderFacets();
    notifySearchChange();
  }, menuToggle);
  form.append(menuPanel.element);

  menuToggle.addEventListener("click", e => {
    e.stopPropagation();
    menuPanel.toggle();
  });

  // Autocomplete Dropdown
  const autocompleteList = document.createElement("div");
  autocompleteList.className = "erp-search-autocomplete o_searchview_autocomplete";
  autocompleteList.setAttribute("role", "listbox");
  autocompleteList.tabIndex = -1;
  autocompleteList.style.display = "none";
  form.append(autocompleteList);

  let activeIndex = -1;
  let currentAutocompleteItems: (AutocompleteItem & { isExpanded?: boolean })[] = [];
  const closeAutocomplete = (): void => {
    autocompleteList.style.display = "none";
    autocompleteList.replaceChildren();
    activeIndex = -1;
    currentAutocompleteItems = [];
  };

  const renderAutocomplete = (rawItems: AutocompleteItem[]): void => {
    currentAutocompleteItems = rawItems;
    autocompleteList.replaceChildren();
    if (!currentAutocompleteItems.length) {
      autocompleteList.style.display = "none";
      activeIndex = -1;
      return;
    }

    currentAutocompleteItems.forEach((item, index) => {
      const row = document.createElement("div");
      row.className = `erp-autocomplete-item ${item.isChild ? "erp-autocomplete-child" : ""} ${index === activeIndex ? "active" : ""}`;
      row.setAttribute("role", "option");
      row.setAttribute("aria-selected", String(index === activeIndex));

      const labelSpan = document.createElement("span");
      labelSpan.className = "erp-autocomplete-label";
      if (item.isChild) {
        labelSpan.innerHTML = `<i>${escapeHtml(item.label)}</i>`;
      } else {
        labelSpan.innerHTML = `Search <b>${escapeHtml(item.fieldLabel)}</b> for: <i class="text-primary">${escapeHtml(item.display ?? item.searchTerm)}</i>`;
      }
      if (item.isParent) {
        const toggle = document.createElement("button");
        toggle.type = "button";
        toggle.className = "erp-autocomplete-expand";
        toggle.setAttribute("aria-label", `${item.isExpanded ? "Hide" : "Show"} matching ${item.fieldLabel} records`);
        toggle.setAttribute("aria-expanded", String(Boolean(item.isExpanded)));
        toggle.textContent = item.isExpanded ? "▾" : "▸";
        toggle.addEventListener("mousedown", event => event.preventDefault());
        toggle.addEventListener("click", event => { event.stopPropagation(); activeIndex = index; void toggleChildren(item); });
        row.append(toggle);
      } else if (item.isChild) {
        row.append(Object.assign(document.createElement("span"), { className: "erp-autocomplete-indent" }));
      }
      row.append(labelSpan);

      row.addEventListener("mouseenter", () => {
        activeIndex = index;
        updateActiveRow();
      });

      row.addEventListener("click", () => {
        selectAutocompleteItem(item);
      });

      autocompleteList.append(row);
    });

    autocompleteList.style.display = "block";
  };

  let childGeneration = 0;
  const toggleChildren = async (item: AutocompleteItem & { isExpanded?: boolean }): Promise<void> => {
    if (item.isExpanded) {
      item.isExpanded = false;
      renderAutocomplete(currentAutocompleteItems.filter(other => other.parentId !== item.id));
      return;
    }
    const request = ++childGeneration, typed = input.value;
    try {
      const children = await model.loadChildren(item);
      if (request !== childGeneration || input.value !== typed || !currentAutocompleteItems.includes(item)) return;
      item.isExpanded = true;
      const at = currentAutocompleteItems.indexOf(item);
      const shown = children.length ? children : [{ ...item, id: `${item.id}_none`, label: "No matching records.", isParent: false, isChild: true, parentId: item.id, searchTerm: "" }];
      renderAutocomplete([...currentAutocompleteItems.slice(0, at + 1), ...shown, ...currentAutocompleteItems.slice(at + 1)]);
    } catch (error) { showError(error); }
  };

  const updateActiveRow = (): void => {
    const rows = autocompleteList.querySelectorAll<HTMLElement>(".erp-autocomplete-item");
    rows.forEach((row, i) => {
      if (i === activeIndex) {
        row.classList.add("active");
        row.setAttribute("aria-selected", "true");
        if (typeof row.scrollIntoView === "function") {
          row.scrollIntoView({ block: "nearest" });
        }
      } else {
        row.classList.remove("active");
        row.setAttribute("aria-selected", "false");
      }
    });
  };

  const selectAutocompleteItem = (item: AutocompleteItem): void => {
    if (item.isChild && item.searchTerm === "") return;
    try { model.addFieldFacet(item.fieldName, item.searchTerm, item.operator, item.display); }
    catch (error) { showError(error); return; }
    input.value = "";
    closeAutocomplete();
    renderFacets();
    notifySearchChange();
  };

  const renderFacets = (): void => {
    chipsContainer.replaceChildren();
    const facets = model.getFacets();
    clearBtn.style.display = facets.length ? "inline-block" : "none";

    for (const facet of facets) {
      const chip = document.createElement("span");
      chip.className = `search-facet-chip erp-facet-chip erp-facet-${facet.type}`;
      chip.tabIndex = 0;
      chip.setAttribute("role", "listitem");

      // Label or icon badge
      const badge = document.createElement("span");
      badge.className = "erp-facet-badge";

      if (facet.type === "filter") {
        badge.innerHTML = `
          <svg class="erp-chip-icon" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
            <path fill="currentColor" d="M10 18h4v-2h-4v2zM3 6v2h18V6H3zm3 7h12v-2H6v2z"/>
          </svg>
        `;
      } else if (facet.type === "groupBy") {
        badge.innerHTML = `
          <svg class="erp-chip-icon" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
            <path fill="currentColor" d="M3 13h2v-2H3v2zm0 4h2v-2H3v2zm0-8h2V7H3v2zm4 4h14v-2H7v2zm0 4h14v-2H7v2zM7 7v2h14V7H7z"/>
          </svg>
        `;
      } else if (facet.type === "favorite") {
        badge.innerHTML = `
          <svg class="erp-chip-icon" viewBox="0 0 24 24" width="12" height="12" aria-hidden="true">
            <path fill="currentColor" d="M12 17.27L18.18 21l-1.64-7.03L22 9.24l-7.19-.61L12 2 9.19 8.63 2 9.24l5.46 4.73L5.82 21z"/>
          </svg>
        `;
      } else {
        badge.textContent = facet.title;
      }

      // Values joined with separator
      const valuesText = document.createElement("span");
      valuesText.className = "chip-text erp-facet-values";

      if (facet.type === "field") {
        // e.g. Name: "Meeting" or "Meeting" or "Project"
        valuesText.textContent = `${facet.title}: ${facet.values.map(v => `"${model.valueLabel(facet.fieldName!, facet.operator ?? "ilike", v)}"`).join(` ${facet.separator} `)}`;
      } else {
        valuesText.textContent = facet.values.join(` ${facet.separator} `);
      }

      if (facet.type === "field" && facet.fieldName) {
        chip.style.cursor = "pointer";
        chip.title = "Click to edit filter";
        chip.addEventListener("click", e => {
          const target = e.target as HTMLElement;
          if (target.closest(".chip-remove-btn")) return;
          const oldVal = facet.values[facet.values.length - 1] || "";
          editingFacetId = facet.id;
          fieldSelect.value = facet.fieldName!;
          closeAutocomplete();
          input.value = oldVal;
          input.focus();
          renderFacets();
        });
      }

      // Remove button ✕
      const removeBtn = document.createElement("button");
      removeBtn.type = "button";
      removeBtn.className = "chip-remove-btn erp-facet-remove";
      removeBtn.setAttribute("aria-label", "Remove filter");
      removeBtn.textContent = "✕";
      removeBtn.addEventListener("click", e => {
        e.stopPropagation();
        model.removeFacet(facet.id);
        renderFacets();
        notifySearchChange();
      });

      chip.append(badge, valuesText, removeBtn);
      chipsContainer.append(chip);
    }
  };

  // Keyboard and Input event handlers
  input.addEventListener("input", () => {
    const term = input.value.trim();
    if (editingFacetId) return;
    if (term) {
      try {
        const items = model.getAutocompleteItems(term);
        activeIndex = items.length ? 0 : -1;
        renderAutocomplete(items);
      } catch (error) { closeAutocomplete(); showError(error); }
    } else {
      closeAutocomplete();
    }
  });

  input.addEventListener("keydown", e => {
    if (e.key === "Backspace" && input.value === "") {
      // Backspace on empty input removes the last facet
      const removed = model.removeLastFacet();
      if (removed) {
        renderFacets();
        notifySearchChange();
      }
      return;
    }

    if (autocompleteList.style.display !== "none" && currentAutocompleteItems.length) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        activeIndex = (activeIndex + 1) % currentAutocompleteItems.length;
        updateActiveRow();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        activeIndex = (activeIndex - 1 + currentAutocompleteItems.length) % currentAutocompleteItems.length;
        updateActiveRow();
        return;
      }
      const active = currentAutocompleteItems[activeIndex] as (AutocompleteItem & { isExpanded?: boolean }) | undefined;
      if ((e.key === "ArrowRight" && active?.isParent && !active.isExpanded) || (e.key === "ArrowLeft" && active?.isParent && active.isExpanded)) {
        e.preventDefault();
        void toggleChildren(active);
        return;
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        const selected = activeIndex >= 0 ? currentAutocompleteItems[activeIndex] : currentAutocompleteItems[0];
        if (selected) {
          selectAutocompleteItem(selected);
        }
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        closeAutocomplete();
        input.value = "";
        return;
      }
    } else if (e.key === "Escape") {
      editingFacetId = undefined;
      input.value = "";
      menuPanel.close();
    }
  });

  // Submit form handler
  form.addEventListener("submit", e => {
    e.preventDefault();
    const term = input.value.trim();
    if (!term) return;

    const activeItem = currentAutocompleteItems[activeIndex];
    if (autocompleteList.style.display !== "none" && activeIndex >= 0 && activeItem) {
      selectAutocompleteItem(activeItem);
      return;
    }

    // Determine target field from fieldSelect or default
    let fieldName = fieldSelect.value;
    if (!fieldName || !model.fields.has(fieldName)) {
      const searchable = model.getSearchableFields();
      const targetField = searchable.find(f => f.name === "name" || f.name === "display_name") || searchable[0];
      fieldName = targetField ? targetField.name : "name";
    }

    try {
      if (editingFacetId) model.editFieldFacet(editingFacetId, term);
      else model.addFieldFacet(fieldName, term);
    } catch (error) { showError(error); return; }
    editingFacetId = undefined;
    input.value = "";
    closeAutocomplete();
    renderFacets();
    notifySearchChange();
  });

  // Document click listener to close autocomplete when clicking outside
  const handleDocClick = (e: MouseEvent): void => {
    if (!form.contains(e.target as globalThis.Node)) {
      closeAutocomplete();
    }
  };
  document.addEventListener("click", handleDocClick);

  const unsubscribe = model.subscribe(() => {
    renderFacets();
  });

  // Initial render
  renderFacets();

  const dispose = (): void => {
    document.removeEventListener("click", handleDocClick);
    unsubscribe();
    menuPanel.dispose();
    form.remove();
  };

  return { element: form, input, form, fieldSelect, render: renderFacets, dispose };
}

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}

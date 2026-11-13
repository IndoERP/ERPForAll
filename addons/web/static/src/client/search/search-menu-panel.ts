/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createHotkeyBoundary } from "@erpforall/elang";
import { reportHotkeyError } from "../../../../../../web/src/runtime/hotkeys.js";
import type { SearchModel } from "./search-model.js";
import { GROUP_INTERVALS } from "./types.js";

let menuSequence = 0;
export function createSearchMenuPanel(model: SearchModel, onUpdate: () => void, trigger?: HTMLButtonElement): {
  element: HTMLElement;
  open(): void;
  close(): void;
  isOpen(): boolean;
  toggle(): void;
  dispose(): void;
} {
  const panel = document.createElement("div");
  panel.className = "erp-search-menu-panel";
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", "Search filters");
  panel.tabIndex = -1;
  panel.id = `search-menu-${++menuSequence}`;
  if (trigger) {
    trigger.setAttribute("aria-haspopup", "dialog");
    trigger.setAttribute("aria-controls", panel.id);
    trigger.setAttribute("aria-expanded", "false");
  }
  panel.style.display = "none";

  let customFilterOpen = false;
  const errorNotice = document.createElement("p");
  errorNotice.setAttribute("role", "alert");
  errorNotice.hidden = true;
  const apply = (change: () => void): void => {
    try { change(); errorNotice.hidden = true; render(); onUpdate(); }
    catch (error) {
      errorNotice.hidden = false;
      errorNotice.textContent = error instanceof Error ? error.message : "Unable to update this filter.";
    }
  };

  const render = (): void => {
    panel.replaceChildren();

    const container = document.createElement("div");
    container.className = "erp-search-menu-sections";

    // 1. FILTERS SECTION
    const filtersSection = document.createElement("div");
    filtersSection.className = "erp-search-section erp-filters-section";

    const filterHeader = document.createElement("div");
    filterHeader.className = "erp-search-section-header";
    filterHeader.innerHTML = `
      <svg class="erp-section-icon" viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
        <path fill="currentColor" d="M10 18h4v-2h-4v2zM3 6v2h18V6H3zm3 7h12v-2H6v2z"/>
      </svg>
      <span>Filters</span>
    `;
    filtersSection.append(filterHeader);

    const filterList = document.createElement("div");
    filterList.className = "erp-search-section-list";

    for (const filter of model.getFilters()) {
      if (filter.type === "dateFilter" && filter.dateOptions) {
        // Date filter with options
        const dateGroup = document.createElement("div");
        dateGroup.className = "erp-search-item erp-search-item-group";

        const title = document.createElement("div");
        title.className = "erp-search-item-title";
        title.textContent = filter.description;
        dateGroup.append(title);

        for (const opt of filter.dateOptions) {
          const optRow = document.createElement("button");
          optRow.type = "button";
          optRow.className = `erp-search-menu-item ${opt.isActive ? "active" : ""}`;
          optRow.setAttribute("role", "checkbox");
          optRow.setAttribute("aria-checked", String(opt.isActive));

          const check = document.createElement("span");
          check.className = "erp-check-icon";
          check.classList.toggle("is-on", opt.isActive);

          const label = document.createElement("span");
          label.className = "erp-item-label";
          label.textContent = opt.label;

          optRow.append(check, label);
          optRow.addEventListener("click", () => {
            apply(() => model.toggleDateOption(filter.id, opt.id));
          });
          dateGroup.append(optRow);
        }
        filterList.append(dateGroup);
      } else {
        const itemRow = document.createElement("button");
        itemRow.type = "button";
        itemRow.className = `erp-search-menu-item ${filter.isActive ? "active" : ""}`;
        itemRow.setAttribute("role", "checkbox");
        itemRow.setAttribute("aria-checked", String(filter.isActive));

        const check = document.createElement("span");
        check.className = "erp-check-icon";
        check.classList.toggle("is-on", filter.isActive);

        const label = document.createElement("span");
        label.className = "erp-item-label";
        label.textContent = filter.description;

        itemRow.append(check, label);
        itemRow.addEventListener("click", () => {
          apply(() => model.toggleFilter(filter.id));
        });
        filterList.append(itemRow);
      }
    }

    // Custom Filter Toggle & Form
    const divider1 = document.createElement("hr");
    divider1.className = "erp-menu-divider";
    filterList.append(divider1);

    const customFilterToggle = document.createElement("button");
    customFilterToggle.type = "button";
    customFilterToggle.className = "erp-search-toggle-form-btn";
    customFilterToggle.textContent = customFilterOpen ? "− Custom Filter" : "+ Add Custom Filter";
    customFilterToggle.addEventListener("click", () => {
      customFilterOpen = !customFilterOpen;
      render();
    });
    filterList.append(customFilterToggle);

    if (customFilterOpen) {
      const customForm = document.createElement("form");
      customForm.className = "erp-custom-filter-form";

      const fieldSelect = document.createElement("select");
      fieldSelect.className = "erp-custom-field-select";
      fieldSelect.setAttribute("aria-label", "Custom filter field");
      for (const [name, field] of model.fields.entries()) {
        const opt = new Option(field.label || name, name);
        fieldSelect.append(opt);
      }

      const opSelect = document.createElement("select");
      opSelect.className = "erp-custom-op-select";
      opSelect.setAttribute("aria-label", "Custom filter operator");
      opSelect.append(
        new Option("contains", "ilike"),
        new Option("is equal to", "="),
        new Option("is not equal to", "!="),
        new Option("greater than", ">"),
        new Option("less than", "<"),
        new Option("greater or equal", ">="),
        new Option("less or equal", "<=")
      );

      const valInput = document.createElement("input");
      valInput.className = "erp-custom-val-input";
      valInput.type = "text";
      valInput.placeholder = "Value…";
      valInput.setAttribute("aria-label", "Custom filter value");

      const applyBtn = document.createElement("button");
      applyBtn.type = "submit";
      applyBtn.className = "erp-btn erp-btn-sm erp-btn-primary";
      applyBtn.textContent = "Apply";

      const note = document.createElement("p");
      note.className = "erp-custom-filter-note muted";
      note.hidden = true;

      customForm.append(fieldSelect, opSelect, valInput, applyBtn, note);
      customForm.addEventListener("submit", e => {
        e.preventDefault();
        e.stopPropagation();
        const fieldName = fieldSelect.value;
        const op = opSelect.value;
        const val = valInput.value.trim();
        if (fieldName && val) {
          try {
            model.addCustomFilter(fieldName, op, val);
            customFilterOpen = false;
            render();
            onUpdate();
          } catch (error) {
            note.setAttribute("role", "alert");
            note.hidden = false;
            note.textContent = error instanceof Error ? error.message : "Unable to apply this filter.";
          }
        }
      });
      filterList.append(customForm);
    }

    filtersSection.append(filterList);
    container.append(filtersSection);
    if (model.hasGroupBy()) container.append(groupBySection());

    panel.append(container, errorNotice);
  };

  // The Group By section: the search view's declared items, then "Add Custom Group". The catalogue is loaded
  // the first time the menu opens, so a list that is never grouped costs no request.
  let groupByLoading = false;
  const groupBySection = (): HTMLElement => {
    const section = document.createElement("div");
    section.className = "erp-search-section erp-groupby-section";
    const header = document.createElement("div");
    header.className = "erp-search-section-header";
    header.innerHTML = `
      <svg class="erp-section-icon" viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
        <path fill="currentColor" d="M3 13h2v-2H3v2zm0 4h2v-2H3v2zm0-8h2V7H3v2zm4 4h14v-2H7v2zm0 4h14v-2H7v2zM7 7v2h14V7H7z"/>
      </svg>
      <span>Group By</span>
    `;
    section.append(header);
    const list = document.createElement("div");
    list.className = "erp-search-section-list";
    section.append(list);
    if (!model.groupByReady()) {
      const status = document.createElement("p");
      status.className = "erp-search-section-status muted";
      status.setAttribute("role", "status");
      status.textContent = groupByLoading ? "Loading group-by choices…" : "Group-by choices are unavailable.";
      list.append(status);
      return section;
    }
    for (const item of model.getGroupBys()) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = `erp-search-menu-item ${item.isActive ? "active" : ""}`;
      row.setAttribute("role", "checkbox");
      row.setAttribute("aria-checked", String(item.isActive));
      const check = document.createElement("span");
      check.className = "erp-check-icon";
      check.classList.toggle("is-on", item.isActive);
      const label = document.createElement("span");
      label.className = "erp-item-label";
      label.textContent = item.description;
      row.append(check, label);
      row.addEventListener("click", () => apply(() => model.toggleGroupBy(item.id)));
      list.append(row);
    }
    const fields = model.getGroupableFields();
    if (fields.length) {
      const select = document.createElement("select");
      select.className = "erp-custom-group-select";
      select.setAttribute("aria-label", "Add custom group");
      select.append(new Option("Add Custom Group", ""));
      for (const field of fields) {
        // A date field offers intervals when its owner enables temporal grouping.
        if (model.hasGroupByIntervals() && ((field.kind as string) === "date" || (field.kind as string) === "datetime")) {
          const dates = document.createElement("optgroup");
          dates.label = field.label;
          for (const [interval, label] of GROUP_INTERVALS) dates.append(new Option(label, `${field.name}:${interval}`));
          select.append(dates);
        } else select.append(new Option(field.label, field.name));
      }
      select.addEventListener("change", () => {
        const chosen = select.value;
        select.value = "";
        if (chosen) apply(() => model.addCustomGroupBy(chosen));
      });
      list.append(select);
    }
    const skipped = model.getSkippedGroupBys();
    if (skipped.length) {
      const note = document.createElement("p");
      note.className = "erp-search-section-status muted";
      note.textContent = `Not offered here: ${skipped.map(item => `${item.description} (${item.reason})`).join("; ")}.`;
      list.append(note);
    }
    return section;
  };
  const loadGroupBy = (): void => {
    if (!model.hasGroupBy() || model.groupByReady() || groupByLoading) return;
    groupByLoading = true;
    errorNotice.hidden = true;
    model.loadGroupByCatalogue().then(() => undefined, error => {
      errorNotice.hidden = false;
      errorNotice.textContent = error instanceof Error ? error.message : "Unable to load the group-by choices.";
    }).finally(() => { groupByLoading = false; if (isOpen()) render(); });
  };

  const handleDocumentClick = (e: MouseEvent): void => {
    if (panel.style.display !== "none" && !e.composedPath().includes(panel)) {
      // Check if click target was the toggler button
      const target = e.target as HTMLElement;
      if (!target.closest(".search-menu-toggle")) {
        close();
      }
    }
  };

  let hotkeys: ReturnType<typeof createHotkeyBoundary> | undefined;

  const fitViewport = (): void => {
    const anchor = panel.parentElement?.getBoundingClientRect();
    if (!anchor) return;
    // Fix to viewport coordinates: an absolute child's translated position drifts when
    // adjacent toolbar tools move its anchor without changing the anchor's size.
    panel.style.position = "fixed";
    panel.style.transform = "";
    panel.style.right = "auto";
    const width = panel.getBoundingClientRect().width;
    const left = Math.max(16, Math.min(anchor.right - width, window.innerWidth - width - 16));
    panel.style.left = `${left}px`;
    panel.style.top = `${Math.max(16, anchor.bottom + 6)}px`;
  };

  // A toolbar can reflow after viewport resize (fonts, facets and adjacent tools).
  // Fit after its actual layout change as well as the window event.
  const anchorResize = typeof ResizeObserver === "function" ? new ResizeObserver(() => {
    if (panel.style.display !== "none") fitViewport();
  }) : undefined;
  const open = (): void => {
    hotkeys?.dispose(); hotkeys = undefined;
    loadGroupBy();
    render();
    panel.style.display = "block";
    trigger?.setAttribute("aria-expanded", "true");
    try { hotkeys = createHotkeyBoundary(panel, { report: reportHotkeyError, onEscape: () => { close(); trigger?.focus(); } }); }
    catch (error) { close(); throw error; }
    panel.focus();
    fitViewport();
    window.addEventListener("resize", fitViewport);
    window.addEventListener("scroll", fitViewport, true);
    if (panel.parentElement) anchorResize?.observe(panel.parentElement);
    document.addEventListener("click", handleDocumentClick);

  };

  // A chip removed (or the search cleared) while the menu is open must uncheck its item.
  const unsubscribe = model.subscribe(() => { if (isOpen()) render(); });
  const close = (): void => {
    panel.style.display = "none";
    trigger?.setAttribute("aria-expanded", "false");
    window.removeEventListener("resize", fitViewport);
    window.removeEventListener("scroll", fitViewport, true);
    anchorResize?.disconnect();
    document.removeEventListener("click", handleDocumentClick);
    hotkeys?.dispose(); hotkeys = undefined;
  };

  const isOpen = (): boolean => panel.style.display !== "none";

  const toggle = (): void => {
    if (isOpen()) close();
    else open();
  };

  const dispose = (): void => {
    unsubscribe();
    close();
    panel.remove();
  };

  return { element: panel, open, close, isOpen, toggle, dispose };
}

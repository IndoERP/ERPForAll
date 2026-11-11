/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import { ClientError } from "./transport.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

/** Priority widget renders selection choices as a group of stars. */
export function priorityControl(options: FieldControlOptions): FieldControl {
  const { field, value, change } = options;
  const config = field.options ?? {};
  if (field.kind !== "selection" || !field.selection) {
    throw new ClientError("server", `Field ${field.name} with priority widget must be a selection field.`);
  }
  if (config.placeholder !== undefined || config.placeholder_field !== undefined) {
    throw new ClientError("validation", "Widget 'priority' does not support placeholder options.");
  }

  const choices = field.selection;
  const firstChoice = choices[0];
  if (!firstChoice) {
    const emptyContainer = el("div", undefined, "field-priority o_priority");
    return { node: fieldShell(field, emptyContainer, options.viewContext).node, dispose: () => {} };
  }
  const defaultVal = firstChoice[0];

  // The first option (choices[0]) represents the empty / zero-star state.
  // The stars displayed correspond to choices.slice(1).
  const stars = choices.slice(1);
  const firstLabel = firstChoice[1];
  let currentIndex = 0;
  if (value !== null && value !== undefined && value !== false && value !== "") {
    const foundIndex = choices.findIndex(([k]) => k === value);
    if (foundIndex < 0) {
      throw new ClientError("server", `Unknown selection value for ${field.name}.`);
    }
    currentIndex = foundIndex;
  }

  // Tooltip: readonly stars name the current level; editable stars name the level a click
  // would set, which is the first choice when the star is the current one.
  const tooltip = (choiceVal: string, choiceLabel: string): string => {
    if (!field.label || field.label === choiceLabel) return choiceLabel;
    const current = choices[currentIndex];
    if (field.readonly) return current ? current[1] : choiceLabel;
    return `Set ${field.label}: ${choices[currentIndex]?.[0] === choiceVal ? firstLabel : choiceLabel}`;
  };

  const container = el("div", undefined, "field-priority o_priority");
  const shell = fieldShell(field, container, options.viewContext);
  container.setAttribute("role", "radiogroup");
  container.setAttribute("aria-label", field.label);

  if (field.readonly) {
    for (let i = 0; i < stars.length; i++) {
      const item = stars[i];
      if (!item) continue;
      const choiceIdx = i + 1;
      const [choiceVal, choiceLabel] = item;
      const filled = choiceIdx <= currentIndex;
      const star = el("span", filled ? "★" : "☆", "field-priority-star o_priority_star");
      star.setAttribute("role", "radio");
      star.setAttribute("aria-label", choiceLabel);
      star.setAttribute("aria-checked", String(choiceIdx === currentIndex));
      star.dataset["icon"] = "star";
      star.dataset["value"] = choiceVal;
      star.title = tooltip(choiceVal, choiceLabel);
      if (filled) {
        star.classList.add("oi-filled", "is-filled");
      }
      container.append(star);
    }
    return { node: shell.node, dispose: () => {} };
  }

  const starButtons: HTMLButtonElement[] = [];

  function renderStars(activeIdx: number) {
    for (let i = 0; i < stars.length; i++) {
      const choiceIdx = i + 1;
      const btn = starButtons[i];
      if (!btn) continue;
      const isFilled = choiceIdx <= activeIdx;
      btn.textContent = isFilled ? "★" : "☆";
      btn.classList.toggle("oi-filled", isFilled);
      btn.classList.toggle("is-filled", isFilled);
    }
  }

  function updateAriaAndTab() {
    for (let i = 0; i < stars.length; i++) {
      const choiceIdx = i + 1;
      const btn = starButtons[i];
      if (!btn) continue;
      const isCurrent = choiceIdx === currentIndex;
      btn.setAttribute("aria-checked", String(isCurrent));
      btn.tabIndex = isCurrent || (currentIndex === 0 && i === 0) ? 0 : -1;
      const star = stars[i];
      if (star) btn.title = tooltip(star[0], star[1]);
    }
  }

  function clearToFirst() {
    shell.clear();
    currentIndex = 0;
    renderStars(0);
    updateAriaAndTab();
    change(defaultVal);
  }

  function selectOption(val: string) {
    const targetIdx = choices.findIndex(([k]) => k === val);
    if (targetIdx === currentIndex) {
      // Clicking the currently selected star clears it to the first option
      clearToFirst();
    } else {
      shell.clear();
      currentIndex = targetIdx;
      renderStars(targetIdx);
      updateAriaAndTab();
      change(val);
    }
  }

  for (let i = 0; i < stars.length; i++) {
    const item = stars[i];
    if (!item) continue;
    const choiceIdx = i + 1;
    const [choiceVal, choiceLabel] = item;
    const isFilled = choiceIdx <= currentIndex;
    const btn = button(isFilled ? "★" : "☆", () => selectOption(choiceVal), "field-priority-star o_priority_star");
    btn.type = "button";
    btn.setAttribute("role", "radio");
    btn.setAttribute("aria-label", choiceLabel);
    btn.setAttribute("aria-checked", String(choiceIdx === currentIndex));
    btn.dataset["icon"] = "star";
    btn.dataset["value"] = choiceVal;
    btn.title = tooltip(choiceVal, choiceLabel);
    if (isFilled) {
      btn.classList.add("oi-filled", "is-filled");
    }
    btn.tabIndex = choiceIdx === currentIndex || (currentIndex === 0 && i === 0) ? 0 : -1;

    btn.addEventListener("mouseenter", () => renderStars(choiceIdx));
    btn.addEventListener("mouseleave", () => renderStars(currentIndex));

    btn.addEventListener("keydown", (event: KeyboardEvent) => {
      if (event.key === "ArrowRight" || event.key === "ArrowUp") {
        event.preventDefault();
        if (i === stars.length - 1) return; // already on the last star: nothing to move to
        const nextIdx = i + 1;
        const nextBtn = starButtons[nextIdx];
        const nextStar = stars[nextIdx];
        if (nextBtn && nextStar) {
          nextBtn.focus();
          selectOption(nextStar[0]);
        }
      } else if (event.key === "ArrowLeft" || event.key === "ArrowDown") {
        event.preventDefault();
        if (i === 0) {
          clearToFirst();
        } else {
          const prevIdx = i - 1;
          const prevBtn = starButtons[prevIdx];
          const prevStar = stars[prevIdx];
          if (prevBtn && prevStar) {
            prevBtn.focus();
            selectOption(prevStar[0]);
          }
        }
      }
    });

    starButtons.push(btn);
    container.append(btn);
  }

  return { node: shell.node, dispose: () => {} };
}

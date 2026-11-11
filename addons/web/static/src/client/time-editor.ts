/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { assertEditorValue, editorLocaleFor, formatTimeValue, parseTimeValue, timeSuggestions, type EditorLocale, type EditorValue } from "./datetime-values.js";
import { el } from "./dom.js";
import { PopoverService, type PopoverHandle } from "./popover.js";
import { ClientError } from "./transport.js";
import { editorConfigurationError } from "./editor-configuration-error.js";

export interface TimeEditorOptions {
  name: string; label: string; value: EditorValue; locale: EditorLocale; popovers: PopoverService;
  readonly?: boolean; required?: boolean; rounding?: number; showSeconds?: boolean; embedded?: boolean;
  signal?: AbortSignal; onApply(value: EditorValue): void; onInvalid?(error: ClientError): void; report(error: unknown): void;
}
export interface TimeEditorHandle { node: HTMLElement; input: HTMLInputElement; setValue(value: EditorValue): void; dispose(): void }
let timeIdentity = 0;

/** Reusable time editor; the owner remains responsible for durable changes. */
export function createTimeEditor(options: TimeEditorOptions): TimeEditorHandle {
  if (!options || typeof options !== "object" || Object.keys(options).some(key => !["name", "label", "value", "locale", "popovers", "readonly", "required", "rounding", "showSeconds", "embedded", "signal", "onApply", "onInvalid", "report"].includes(key))) throw new ClientError("validation", "Invalid time editor options.");
  if (typeof options.name !== "string" || !options.name || options.name.length > 256 || typeof options.label !== "string" || options.label.length > 256 || !(options.popovers instanceof PopoverService) || typeof options.onApply !== "function" || typeof options.report !== "function" || options.onInvalid !== undefined && typeof options.onInvalid !== "function") throw new ClientError("validation", "Invalid time editor options.");
  for (const key of ["readonly", "required", "showSeconds", "embedded"] as const) if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ClientError("validation", "Invalid time editor options.");
  if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation", "Invalid time editor signal.");
  const showSeconds = options.showSeconds ?? false;
  const suggestions = timeSuggestions(options.rounding);
  assertEditorValue("time", options.value);
  let locale: EditorLocale;
  try { locale = editorLocaleFor("time", options.locale); }
  catch (failure) {
    if (!(failure instanceof ClientError) || failure.kind !== "unsupported") throw failure;
    return editorConfigurationError({ ...options, kind: "time" }, failure);
  }
  let represented = options.value, dead = options.signal?.aborted === true, overlay: PopoverHandle | undefined, embeddedList: HTMLElement | undefined;
  let active = -1, small = window.innerWidth <= 700, returningFocus = false;
  const node = el("div", undefined, "field-label time-editor"), label = el("label", options.label);
  const input = el("input"), error = el("small", undefined, "editor-validation");
  const identity = `time-editor-${++timeIdentity}`; label.id = `${identity}-label`; label.htmlFor = identity; input.id = identity;
  input.name = options.name; input.maxLength = 256; input.required = options.required ?? false; input.disabled = options.readonly === true || dead;
  input.setAttribute("aria-labelledby", label.id); error.id = `${identity}-error`; error.hidden = true; error.setAttribute("role", "status");
  input.setAttribute("aria-describedby", error.id);
  if (options.required) { node.classList.add("field-required"); const marker = el("span", "*", "field-required-marker"); marker.setAttribute("aria-hidden", "true"); label.append(" ", marker); }
  if (options.readonly) node.classList.add("field-readonly");
  node.append(label, input, error);
  const enabled = (): boolean => !dead && !options.readonly;
  const focusInput = (): void => { if (!enabled() || !input.isConnected) return; returningFocus = true; try { input.focus(); } finally { returningFocus = false; } };
  const report = (job: () => void): void => { try { job(); } catch (failure) { options.report(failure); } };
  const display = (value: EditorValue): string => small ? value === false ? "" : value.slice(0, showSeconds ? 8 : 5) : formatTimeValue(value, locale, showSeconds);
  const clearError = (): void => { input.removeAttribute("aria-invalid"); input.setCustomValidity(""); error.hidden = true; error.textContent = ""; };
  const invalid = (failure: ClientError, notify: boolean): void => {
    input.setAttribute("aria-invalid", "true"); input.setCustomValidity(failure.message); error.textContent = failure.message; error.hidden = false;
    if (notify) options.onInvalid?.(failure);
  };
  const restore = (): void => { if (dead) return; input.value = display(represented); active = -1; clearError(); };
  const clearList = (): void => { embeddedList?.remove(); embeddedList = undefined; input.setAttribute("aria-expanded", "false"); input.removeAttribute("aria-controls"); input.removeAttribute("aria-activedescendant"); };
  const close = (restoreValue: boolean): void => {
    const current = overlay; overlay = undefined;
    try { current?.close(); } finally { clearList(); if (restoreValue) restore(); }
  };
  const apply = (value: EditorValue): void => {
    if (!enabled()) return;
    const previous = represented; represented = value; input.value = display(value); clearError();
    close(false);
    if (previous === value) return;
    try { options.onApply(value); } catch (failure) { if (!dead) { represented = previous; restore(); } throw failure; }
  };
  const parse = (notify: boolean): void => {
    if (!enabled()) return;
    if (small && input.validity.badInput) { close(false); invalid(new ClientError("validation", "Enter a valid time."), notify); return; }
    const result = parseTimeValue(input.value, locale, { previous: represented, showSeconds, ...(options.rounding === undefined ? {} : { rounding: options.rounding }) });
    if (result.ok) apply(result.value); else { close(false); invalid(result.error, notify); }
  };
  const markActive = (): void => {
    const list = embeddedList ?? overlay?.node.querySelector<HTMLElement>('.time-suggestions'); if (!list) return;
    for (const [index, option] of [...list.querySelectorAll<HTMLElement>('[role="option"]')].entries()) option.setAttribute("aria-selected", String(index === active));
    const selected = list.querySelector<HTMLElement>(`#${identity}-option-${active}`);
    if (selected) { input.setAttribute("aria-activedescendant", selected.id); list.setAttribute("aria-activedescendant", selected.id); selected.scrollIntoView?.({ block: "nearest" }); }
    else { input.removeAttribute("aria-activedescendant"); list.removeAttribute("aria-activedescendant"); }
  };
  const open = (allowInvalid = false): void => {
    if (!enabled() || small || overlay || embeddedList) return;
    const parsed = parseTimeValue(input.value, locale, { previous: represented, showSeconds });
    if (!parsed.ok && !allowInvalid) return;
    const list = el("div", undefined, "time-suggestions");
    list.id = `${identity}-list`; list.setAttribute("role", "listbox"); list.setAttribute("aria-label", `${options.label} suggestions`); list.tabIndex = 0; list.addEventListener("keydown", onKey);
    active = parsed.ok && parsed.value !== false ? suggestions.indexOf(showSeconds ? parsed.value : `${parsed.value.slice(0, 5)}:00`) : -1;
    for (const [index, value] of suggestions.entries()) {
      const option = el("button", formatTimeValue(value, locale, showSeconds), "time-suggestion"); option.type = "button"; option.tabIndex = -1;
      option.id = `${identity}-option-${index}`; option.setAttribute("role", "option"); option.setAttribute("aria-selected", String(index === active));
      option.addEventListener("pointerdown", event => event.preventDefault());
      option.addEventListener("click", () => report(() => {
        const result = parseTimeValue(formatTimeValue(value, locale, showSeconds), locale, { previous: represented, showSeconds, ...(options.rounding === undefined ? {} : { rounding: options.rounding }) });
        if (result.ok) { apply(result.value); focusInput(); } else invalid(result.error, true);
      })); list.append(option);
    }
    if (options.embedded) { embeddedList = list; node.append(list); }
    else {
      overlay = options.popovers.open(input, list, { role: "region", ...(options.signal ? { signal: options.signal } : {}), onClose: reason => {
        overlay = undefined; clearList();
        if (reason === "outside") report(() => parse(false));
        else if (reason === "escape" || reason === "navigation" || reason === "abort" || reason === "anchor-removed" || reason === "dispose") { restore(); if (reason === "escape") focusInput(); }
      } });
      overlay.node.setAttribute("aria-label", `${options.label} suggestions`);
    }
    input.setAttribute("aria-expanded", "true"); input.setAttribute("aria-controls", list.id); markActive();
  };
  const onInput = (): void => report(() => {
    if (!enabled()) return; const result = parseTimeValue(input.value, locale, { previous: represented, showSeconds });
    if (result.ok) { clearError(); open(); } else { close(false); invalid(result.error, false); return; }
    active = result.ok && result.value !== false ? suggestions.indexOf(showSeconds ? result.value : `${result.value.slice(0, 5)}:00`) : -1; markActive();
  });
  const onChange = (): void => report(() => parse(true));
  const onFocus = (): void => { if (!returningFocus) report(open); };
  const onKey = (event: KeyboardEvent): void => report(() => {
    if (!enabled() || event.altKey || event.ctrlKey || event.metaKey) return;
    const fromList = event.currentTarget !== input;
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(true); if (fromList) focusInput(); return; }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      if (small) return; event.preventDefault(); open(true);
      active = active < 0 ? event.key === "ArrowDown" ? 0 : suggestions.length - 1 : (active + (event.key === "ArrowDown" ? 1 : -1) + suggestions.length) % suggestions.length;
      const value = suggestions[active]; if (value === undefined) throw new ClientError("server", "Time suggestion index is invalid.");
      input.value = display(value); clearError(); markActive(); return;
    }
    if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); parse(true); if (fromList) focusInput(); }
    else if (event.key === "Tab") {
      if (active >= 0) parse(true); close(false);
      // The list is removed before native Tab runs. Re-establish the combobox as its logical
      // origin, without reopening it, so forward/backward departure uses the field's tab order.
      if (fromList) focusInput();
    }
  });
  const onBlur = (event: FocusEvent): void => {
    const next = event.relatedTarget;
    if (next instanceof Node && (node.contains(next) || overlay?.node.contains(next))) return;
    // Native change owns typed commits; blur only releases suggestions and never silently validates invalid text.
    close(false);
  };
  const onResize = (): void => report(() => {
    const next = window.innerWidth <= 700; if (small === next) return; close(true); small = next; route();
  });
  const route = (): void => {
    input.type = small ? "time" : "text"; input.step = showSeconds ? "1" : "60";
    if (small) { input.removeAttribute("role"); input.removeAttribute("aria-autocomplete"); input.removeAttribute("aria-expanded"); }
    else { input.setAttribute("role", "combobox"); input.setAttribute("aria-autocomplete", "list"); input.setAttribute("aria-expanded", "false"); }
    input.value = display(represented);
  };
  // Validate the represented value even on the numeric phone route, which otherwise only slices strings.
  formatTimeValue(represented, locale, showSeconds); route();
  const dispose = (): void => {
    if (dead) return; dead = true; const current = overlay; overlay = undefined;
    try { current?.close("dispose"); } finally {
      clearList(); options.signal?.removeEventListener("abort", dispose); window.removeEventListener("resize", onResize);
      input.removeEventListener("input", onInput); input.removeEventListener("change", onChange); input.removeEventListener("focus", onFocus); input.removeEventListener("click", onFocus); input.removeEventListener("keydown", onKey); input.removeEventListener("blur", onBlur);
    }
  };
  if (!dead) {
    input.addEventListener("input", onInput); input.addEventListener("change", onChange); input.addEventListener("focus", onFocus); input.addEventListener("click", onFocus); input.addEventListener("keydown", onKey); input.addEventListener("blur", onBlur);
    window.addEventListener("resize", onResize); options.signal?.addEventListener("abort", dispose, { once: true });
  }
  return { node, input, setValue: value => { if (dead) throw new ClientError("validation", "Time editor is disposed."); formatTimeValue(value, locale, showSeconds); represented = value; close(true); }, dispose };
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { assertEditorValue, editorLocaleFor, formatEditorValue, parseEditorValue, parseTimeValue, partsFromValue, timeSuggestions, valueFromParts, type DateKind, type DateParts, type EditorLocale, type EditorValue, type ParseResult } from "./datetime-values.js";
import { button, el } from "./dom.js";
import { icon } from "./icons.js";
import { PopoverService, type PopoverHandle, type PopoverCloseReason } from "./popover.js";
import { createTimeEditor, type TimeEditorHandle } from "./time-editor.js";
import { ClientError } from "./transport.js";
import { editorConfigurationError } from "./editor-configuration-error.js";

export type DatePickerPrecision = "days" | "months" | "years" | "decades";
const PRECISIONS: readonly DatePickerPrecision[] = ["days", "months", "years", "decades"];

export interface DateTimeEditorOptions {
  kind: DateKind; name: string; label: string; value: EditorValue; locale: EditorLocale; popovers: PopoverService;
  /** Range owners may need a valid unchanged endpoint to release another pending endpoint. */
  notifyUnchanged?: boolean; readonly?: boolean; required?: boolean; min?: string; max?: string; isDateValid?: (isoDate: string) => boolean;
  rounding?: number; showSeconds?: boolean; showTime?: boolean; numeric?: boolean; placeholder?: string;
  minPrecision?: DatePickerPrecision; maxPrecision?: DatePickerPrecision; warn?: "none" | "past" | "future"; signal?: AbortSignal;
  onApply(value: EditorValue): void; onInvalid?(error: ClientError): void; report(error: unknown): void;
}
export interface DateTimeEditorHandle { node: HTMLElement; input: HTMLInputElement; setValue(value: EditorValue): void; dispose(): void }
export interface DateTimeRangeEditorOptions extends Omit<DateTimeEditorOptions, "name" | "label" | "value" | "required" | "onApply"> {
  names: readonly [string, string]; labels: readonly [string, string]; value: readonly [EditorValue, EditorValue];
  required?: readonly [boolean, boolean]; readonlyEndpoints?: readonly [boolean, boolean]; onApply(value: readonly [EditorValue, EditorValue]): void;
}
export interface DateTimeRangeEditorHandle { node: HTMLElement; inputs: readonly [HTMLInputElement, HTMLInputElement]; setValue(value: readonly [EditorValue, EditorValue]): void; dispose(): void }
let dateTimeIdentity = 0;

function calendarDate(year: number, month: number, day: number): Date {
  const result = new Date(0); result.setUTCFullYear(year, month - 1, day); result.setUTCHours(0, 0, 0, 0); return result;
}
function isoDate(value: Date): string { return `${String(value.getUTCFullYear()).padStart(4, "0")}-${String(value.getUTCMonth() + 1).padStart(2, "0")}-${String(value.getUTCDate()).padStart(2, "0")}`; }
function dateFromIso(value: string): Date { return calendarDate(Number(value.slice(0, 4)), Number(value.slice(5, 7)), Number(value.slice(8, 10))); }
function calendarParts(value: Date): Pick<DateParts, "year" | "month" | "day"> { return { year: value.getUTCFullYear(), month: value.getUTCMonth() + 1, day: value.getUTCDate() }; }
function localToday(locale: EditorLocale): DateParts {
  const parts = new Intl.DateTimeFormat("en-CA", { timeZone: locale.tz, year: "numeric", month: "2-digit", day: "2-digit", numberingSystem: "latn" }).formatToParts(new Date());
  const number = (kind: "year" | "month" | "day"): number => {
    const value = parts.find(part => part.type === kind)?.value;
    if (value === undefined || !/^\d+$/.test(value)) throw new ClientError("server", "The current date could not be formatted in the configured timezone.");
    return Number(value);
  };
  return { year: number("year"), month: number("month"), day: number("day"), hour: 0, minute: 0, second: 0 };
}
function canonicalTime(parts: DateParts): string { return [parts.hour, parts.minute, parts.second].map(value => String(value).padStart(2, "0")).join(":"); }

/** Calendar/date-time editor, scoped to the caller and shared popover service. */
export function createDateTimeEditor(options: DateTimeEditorOptions): DateTimeEditorHandle {
  if (!options || typeof options !== "object" || Object.keys(options).some(key => !["kind", "name", "label", "value", "locale", "popovers", "readonly", "required", "notifyUnchanged", "min", "max", "isDateValid", "rounding", "showSeconds", "showTime", "numeric", "placeholder", "minPrecision", "maxPrecision", "warn", "signal", "onApply", "onInvalid", "report"].includes(key))) throw new ClientError("validation", "Invalid date/time editor options.");
  if (options.kind !== "date" && options.kind !== "datetime" || typeof options.name !== "string" || !options.name || options.name.length > 256 || typeof options.label !== "string" || options.label.length > 256 || !(options.popovers instanceof PopoverService) || typeof options.onApply !== "function" || typeof options.report !== "function" || options.onInvalid !== undefined && typeof options.onInvalid !== "function" || options.isDateValid !== undefined && typeof options.isDateValid !== "function") throw new ClientError("validation", "Invalid date/time editor options.");
  for (const key of ["readonly", "required", "showSeconds", "showTime", "numeric", "notifyUnchanged"] as const) if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ClientError("validation", "Invalid date/time editor options.");
  if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation", "Invalid date/time editor signal.");
  if (options.min !== undefined && typeof options.min !== "string") throw new ClientError("validation", "Date/time minimum must be a canonical string.");
  if (options.max !== undefined && typeof options.max !== "string") throw new ClientError("validation", "Date/time maximum must be a canonical string.");
  if (options.placeholder !== undefined && (typeof options.placeholder !== "string" || options.placeholder.length > 256)) throw new ClientError("validation", "Date placeholder must be text of at most 256 characters.");
  if (options.warn !== undefined && options.warn !== "none" && options.warn !== "past" && options.warn !== "future") throw new ClientError("validation", "Date warning must be none, past or future.");
  const minPrecision = options.minPrecision ?? "days", maxPrecision = options.maxPrecision ?? "decades";
  if (!PRECISIONS.includes(minPrecision) || !PRECISIONS.includes(maxPrecision)) throw new ClientError("validation", "Date picker precision must be days, months, years or decades.");
  if (PRECISIONS.indexOf(minPrecision) > PRECISIONS.indexOf(maxPrecision)) throw new ClientError("validation", "Date picker minimum precision must not exceed maximum precision.");
  let precision = minPrecision;
  const kind = options.kind, showSeconds = options.showSeconds ?? options.rounding === 0, showTime = kind === "datetime" && options.showTime !== false;
  timeSuggestions(options.rounding);
  assertEditorValue(kind, options.value);
  if (options.min !== undefined) assertEditorValue(kind, options.min);
  if (options.max !== undefined) assertEditorValue(kind, options.max);
  if (options.min !== undefined && options.max !== undefined && options.min > options.max) throw new ClientError("validation", "Date/time minimum must not be after maximum.");
  let locale: EditorLocale;
  try {
    locale = editorLocaleFor(kind, options.locale);
    if (options.numeric !== undefined) {
      const date_format = locale.date_format.replace(/%(%|m|b|B)/g, (token, directive: string) => directive === "%" ? token : options.numeric ? "%m" : "%B");
      locale = editorLocaleFor(kind, { ...locale, date_format });
    }
  }
  catch (failure) {
    if (!(failure instanceof ClientError) || failure.kind !== "unsupported") throw failure;
    return editorConfigurationError(options, failure);
  }
  const representedParts = partsFromValue(kind, options.value, locale);
  const minimumParts = options.min === undefined ? null : partsFromValue(kind, options.min, locale), maximumParts = options.max === undefined ? null : partsFromValue(kind, options.max, locale);
  const minimumDay = minimumParts === null ? undefined : isoDate(calendarDate(minimumParts.year, minimumParts.month, minimumParts.day));
  const maximumDay = maximumParts === null ? undefined : isoDate(calendarDate(maximumParts.year, maximumParts.month, maximumParts.day));
  let represented = options.value, draft = represented, dead = options.signal?.aborted === true;
  let overlay: PopoverHandle | undefined, calendar: HTMLElement | undefined, time: TimeEditorHandle | undefined;
  const starting = representedParts ?? localToday(locale);
  let viewYear = starting.year, viewMonth = starting.month, activeDate = isoDate(calendarDate(starting.year, starting.month, starting.day));
  const node = el("div", undefined, "field-label datetime-editor"), label = el("label", options.label), area = el("div", undefined, "datetime-input-area field-input-icon");
  const input = el("input"), error = el("small", undefined, "editor-validation"), identity = `datetime-editor-${++dateTimeIdentity}`;
  label.id = `${identity}-label`; label.htmlFor = identity; input.id = identity; input.name = options.name; input.type = "text"; input.maxLength = 256;
  input.placeholder = options.placeholder ?? "";
  input.required = options.required ?? false; input.disabled = options.readonly === true || dead; input.autocomplete = "off";
  input.setAttribute("aria-labelledby", label.id); input.setAttribute("aria-describedby", `${identity}-error`);
  error.id = `${identity}-error`; error.hidden = true; error.setAttribute("role", "status");
  if (options.required) { node.classList.add("field-required"); const marker = el("span", "*", "field-required-marker"); marker.setAttribute("aria-hidden", "true"); label.append(" ", marker); }
  if (options.readonly) node.classList.add("field-readonly");
  const warning = el("small", undefined, "editor-warning"); warning.id = `${identity}-warning`; warning.dataset["dateWarning"] = options.warn ?? "none"; warning.hidden = true; warning.setAttribute("role", "status");
  if (options.warn && options.warn !== "none") input.setAttribute("aria-describedby", `${identity}-error ${identity}-warning`);
  area.append(input); node.append(label, area, error, warning);
  const lang = locale.lang.replaceAll("_", "-");
  const monthFormatter = new Intl.DateTimeFormat(lang, { timeZone: "UTC", calendar: "gregory", numberingSystem: "latn", month: "long", year: "numeric" });
  const dayFormatter = new Intl.DateTimeFormat(lang, { timeZone: "UTC", calendar: "gregory", numberingSystem: "latn", dateStyle: "full" });
  const weekdayFormatter = new Intl.DateTimeFormat(lang, { timeZone: "UTC", calendar: "gregory", weekday: "short" });
  const enabled = (): boolean => !dead && !options.readonly;
  const safely = (job: () => void): void => { try { job(); } catch (failure) { options.report(failure); } };
  const clearError = (): void => { input.removeAttribute("aria-invalid"); input.setCustomValidity(""); error.hidden = true; error.textContent = ""; };
  const invalid = (failure: ClientError): void => {
    input.setAttribute("aria-invalid", "true"); input.setCustomValidity(failure.message); error.textContent = failure.message; error.hidden = false; options.onInvalid?.(failure);
  };
  const display = (value: EditorValue): string => {
    if (kind !== "datetime" || showTime || value === false) return formatEditorValue(kind, value, locale, { showSeconds });
    const parts = partsFromValue(kind, value, locale);
    if (!parts) throw new ClientError("server", "A represented date/time value has no calendar parts.");
    return formatEditorValue("date", isoDate(calendarDate(parts.year, parts.month, parts.day)), locale);
  };
  const parse = (text: string): ParseResult => {
    if (kind !== "datetime" || showTime) return parseEditorValue(kind, text, locale, { previous: represented, showSeconds });
    if (text.trim() === display(represented)) return { ok: true, value: represented };
    const date = parseEditorValue("date", text, locale);
    if (!date.ok || date.value === false) return date;
    const selected = partsFromValue("date", date.value, locale), previous = partsFromValue(kind, represented, locale);
    if (!selected) throw new ClientError("server", "A represented date/time value has no calendar parts.");
    return valueFromParts(kind, { ...selected, hour: previous?.hour ?? 0, minute: previous?.minute ?? 0, second: previous?.second ?? 0 }, locale, represented);
  };
  const refreshWarning = (value: EditorValue): void => {
    warning.textContent = ""; warning.hidden = true;
    if (!options.warn || options.warn === "none" || value === false) return;
    const parts = partsFromValue(kind, value, locale), current = localToday(locale);
    if (!parts) throw new ClientError("server", "A represented date/time value has no calendar parts.");
    const day = isoDate(calendarDate(parts.year, parts.month, parts.day)), today = isoDate(calendarDate(current.year, current.month, current.day));
    if (options.warn === "future" && day > today || options.warn === "past" && day < today) {
      warning.textContent = options.warn === "future" ? "This date is in the future." : "This date is in the past."; warning.hidden = false;
    }
  };
  const restore = (): void => { draft = represented; input.value = display(represented); clearError(); refreshWarning(represented); };
  const allowedDay = (date: string): boolean => {
    const validity = options.isDateValid?.(date);
    if (options.isDateValid !== undefined && typeof validity !== "boolean") throw new ClientError("validation", "Date validity callback must return a boolean.");
    return (minimumDay === undefined || date >= minimumDay) && (maximumDay === undefined || date <= maximumDay) && validity !== false;
  };
  const validValue = (value: EditorValue): boolean => {
    if (value === false) return true;
    const parts = partsFromValue(kind, value, locale);
    if (parts === null) throw new ClientError("server", "A represented date/time value has no calendar parts.");
    const day = isoDate(calendarDate(parts.year, parts.month, parts.day));
    if (!allowedDay(day) || options.min !== undefined && value < options.min || options.max !== undefined && value > options.max) { invalid(new ClientError("validation", "Date/time value is outside the permitted range.")); return false; }
    return true;
  };
  const releaseCalendar = (): void => {
    const current = time; time = undefined; calendar = undefined; input.removeAttribute("aria-controls"); opener?.setAttribute("aria-expanded", "false"); current?.dispose();
  };
  const close = (reason: PopoverCloseReason = "explicit", focus = false): void => {
    const current = overlay, errors: unknown[] = []; overlay = undefined;
    try { current?.close(reason); } catch (failure) { errors.push(failure); }
    try { releaseCalendar(); } catch (failure) { errors.push(failure); }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, "Date/time overlay cleanup failed.");
    if (focus && enabled() && input.isConnected) input.focus();
  };
  const apply = (value: EditorValue, focus: boolean): void => {
    if (!enabled() || !validValue(value)) return;
    const previous = represented; represented = value; draft = value; input.value = display(value); clearError(); refreshWarning(value);
    close("explicit", focus);
    if (value === previous && !options.notifyUnchanged) return;
    try { options.onApply(value); } catch (failure) { if (!dead) { represented = previous; restore(); } throw failure; }
  };
  const parseInput = (focus: boolean): void => {
    if (!enabled()) return;
    const result = parse(input.value);
    if (!result.ok) { restore(); invalid(result.error); return; }
    if (time && !time.input.checkValidity()) { invalid(new ClientError("validation", "Enter a valid time.")); return; }
    apply(result.value, focus);
  };
  const updateDraft = (parts: DateParts): boolean => {
    const result = valueFromParts(kind, parts, locale, represented);
    if (!result.ok) { invalid(result.error); return false; }
    if (!validValue(result.value)) return false;
    draft = result.value; input.value = display(draft); clearError(); return true;
  };
  const selectedParts = (): DateParts => partsFromValue(kind, draft, locale) ?? localToday(locale);
  const changeDate = (date: string): void => {
    if (!enabled() || !allowedDay(date)) return;
    const selected = calendarParts(dateFromIso(date)), previous = selectedParts();
    if (!updateDraft({ ...previous, ...selected })) return;
    activeDate = date;
    if (kind === "date" || !showTime) apply(draft, true);
    else { time?.setValue(canonicalTime(selectedParts())); renderCalendar(false); }
  };
  const moveMonth = (months: number): void => {
    const value = calendarDate(viewYear, viewMonth + months, 1); if (value.getUTCFullYear() < 1 || value.getUTCFullYear() > 9999) return;
    viewYear = value.getUTCFullYear(); viewMonth = value.getUTCMonth() + 1; renderCalendar(false);
  };
  const renderPeriods = (focusDate: boolean): void => {
    if (!calendar || precision === "days" || dead) return;
    const level = PRECISIONS.indexOf(precision), minimum = PRECISIONS.indexOf(minPrecision);
    const step = precision === "months" ? 1 : precision === "years" ? 10 : 100;
    const base = precision === "months" ? viewYear : Math.floor(viewYear / step) * step;
    const header = el("div", undefined, "datetime-calendar-heading");
    const move = (direction: number): void => {
      const next = viewYear + direction * step;
      if (next < 1 || next > 9999) return;
      viewYear = next; renderCalendar(true);
    };
    const previous = button("‹", () => safely(() => move(-1))); previous.setAttribute("aria-label", `Previous ${step === 1 ? "year" : `${step} years`}`);
    const next = button("›", () => safely(() => move(1))); next.setAttribute("aria-label", `Next ${step === 1 ? "year" : `${step} years`}`);
    const text = precision === "months" ? String(viewYear) : `${base}–${base + step - 1}`;
    const title = el("strong", text); title.id = `${identity}-month`; title.setAttribute("aria-live", "polite");
    if (level < PRECISIONS.indexOf(maxPrecision)) {
      const zoom = button(text, () => safely(() => { const higher = PRECISIONS[level + 1]; if (higher) { precision = higher; renderCalendar(true); } }));
      zoom.dataset["editorAction"] = "zoom-out"; zoom.setAttribute("aria-label", precision === "months" ? "Choose year" : "Choose decade"); title.replaceChildren(zoom);
    }
    header.append(previous, title, next);
    const grid = el("table", undefined, "datetime-calendar-grid"); grid.setAttribute("role", "grid"); grid.setAttribute("aria-label", `${options.label} ${precision}`); grid.setAttribute("aria-describedby", title.id);
    const body = el("tbody"), buttons: HTMLButtonElement[] = [];
    const selected = draft === false ? undefined : selectedParts();
    const monthName = new Intl.DateTimeFormat(lang, { timeZone: "UTC", calendar: "gregory", month: "short" });
    let active: HTMLButtonElement | undefined;
    for (let rowIndex = 0; rowIndex < 4; rowIndex++) {
      const row = el("tr");
      for (let column = 0; column < 3; column++) {
        const index = rowIndex * 3 + column;
        const year = precision === "months" ? viewYear : precision === "years" ? base + index - 1 : base + (index - 1) * 10;
        const month = precision === "months" ? index + 1 : 1;
        const years = precision === "decades" ? 10 : 1;
        const start = calendarDate(year, month, 1), end = precision === "months" ? calendarDate(year, month + 1, 0) : calendarDate(year + years, 1, 0);
        const firstDay = isoDate(start), lastDay = isoDate(end);
        const caption = precision === "months" ? monthName.format(start) : precision === "years" ? String(year) : `${year}–${year + 9}`;
        const action = button(caption, () => safely(() => {
          viewYear = year; viewMonth = month;
          if (level > minimum) { const lower = PRECISIONS[level - 1]; if (lower) { precision = lower; renderCalendar(true); } }
          else changeDate(firstDay);
        }), "datetime-day datetime-period");
        action.dataset["periodStart"] = firstDay; action.tabIndex = -1;
        action.disabled = year < 1 || year + years - 1 > 9999 || maximumDay !== undefined && firstDay > maximumDay || minimumDay !== undefined && lastDay < minimumDay || level === minimum && !allowedDay(firstDay);
        const chosen = selected !== undefined && selected.year >= year && selected.year < year + years && (precision !== "months" || selected.month === month);
        const cell = el("td"); cell.setAttribute("aria-selected", String(chosen)); cell.append(action); row.append(cell); buttons.push(action);
        if (!action.disabled && (chosen || active === undefined)) active = action;
      }
      body.append(row);
    }
    if (active) active.tabIndex = 0;
    grid.append(body);
    grid.addEventListener("keydown", event => safely(() => {
      if (!(event.target instanceof HTMLButtonElement) || !buttons.includes(event.target) || event.altKey || event.ctrlKey || event.metaKey) return;
      const index = buttons.indexOf(event.target); let target: number | undefined;
      switch (event.key) {
        case "ArrowLeft": target = index - 1; break;
        case "ArrowRight": target = index + 1; break;
        case "ArrowUp": target = index - 3; break;
        case "ArrowDown": target = index + 3; break;
        case "Home": target = 0; break;
        case "End": target = buttons.length - 1; break;
        case "PageUp": event.preventDefault(); move(-1); return;
        case "PageDown": event.preventDefault(); move(1); return;
        case "Enter": case " ": event.preventDefault(); event.target.click(); return;
      }
      if (target === undefined) return;
      event.preventDefault(); const candidate = buttons[target];
      if (candidate && !candidate.disabled) { for (const button of buttons) button.tabIndex = button === candidate ? 0 : -1; candidate.focus(); }
    }));
    const actions = el("div", undefined, "datetime-calendar-actions");
    const clear = button("Clear", () => safely(() => apply(false, true))); clear.dataset["editorAction"] = "clear";
    const cancel = button("Cancel", () => safely(() => { restore(); close("escape", true); })); cancel.dataset["editorAction"] = "cancel";
    actions.append(clear, cancel);
    if (showTime) { const accept = button("Apply", () => safely(() => parseInput(true)), "primary"); accept.dataset["editorAction"] = "apply"; actions.append(accept); }
    calendar.replaceChildren(header, grid, ...(time ? [time.node] : []), actions);
    if (focusDate) active?.focus();
  };
  const renderCalendar = (focusDate: boolean): void => {
    if (!calendar || dead) return;
    calendar.dataset["calendarPrecision"] = precision;
    if (precision !== "days") { renderPeriods(focusDate); return; }
    const header = el("div", undefined, "datetime-calendar-heading"), title = el("strong", monthFormatter.format(calendarDate(viewYear, viewMonth, 1)));
    title.id = `${identity}-month`; title.setAttribute("aria-live", "polite");
    if (PRECISIONS.indexOf(precision) < PRECISIONS.indexOf(maxPrecision)) {
      const zoom = button(title.textContent ?? "", () => safely(() => { precision = "months"; renderCalendar(true); }));
      zoom.dataset["editorAction"] = "zoom-out"; zoom.setAttribute("aria-label", "Choose month"); title.replaceChildren(zoom);
    }
    for (const [name, text, step] of [["Previous year", "«", -12], ["Previous month", "‹", -1]] as const) { const action = button(text, () => safely(() => moveMonth(step))); action.setAttribute("aria-label", name); header.append(action); }
    header.append(title);
    for (const [name, text, step] of [["Next month", "›", 1], ["Next year", "»", 12]] as const) { const action = button(text, () => safely(() => moveMonth(step))); action.setAttribute("aria-label", name); header.append(action); }
    const grid = el("table", undefined, "datetime-calendar-grid"); grid.setAttribute("role", "grid"); grid.setAttribute("aria-label", `${options.label} calendar`); grid.setAttribute("aria-describedby", title.id);
    const head = el("thead"), headings = el("tr");
    // Monday-first is the component default; the current wire has no configured week-start declaration.
    for (let day = 0; day < 7; day++) { const value = calendarDate(2026, 11, 2 + day), cell = el("th", weekdayFormatter.format(value)); cell.scope = "col"; headings.append(cell); }
    head.append(headings); const body = el("tbody"), first = calendarDate(viewYear, viewMonth, 1), start = 1 - (first.getUTCDay() + 6) % 7;
    const today = localToday(locale), todayDate = isoDate(calendarDate(today.year, today.month, today.day));
    const selected = draft === false ? undefined : selectedParts(), selectedDate = selected === undefined ? undefined : isoDate(calendarDate(selected.year, selected.month, selected.day));
    let activeButton: HTMLButtonElement | undefined, firstAllowed: HTMLButtonElement | undefined;
    for (let week = 0; week < 6; week++) {
      const row = el("tr");
      for (let day = 0; day < 7; day++) {
        const date = calendarDate(viewYear, viewMonth, start + week * 7 + day), dateString = isoDate(date), cell = el("td");
        const action = button(String(date.getUTCDate()), () => safely(() => changeDate(dateString)), "datetime-day");
        action.dataset["date"] = dateString; action.setAttribute("aria-label", dayFormatter.format(date)); action.disabled = date.getUTCFullYear() < 1 || date.getUTCFullYear() > 9999 || !allowedDay(dateString); action.tabIndex = -1;
        cell.setAttribute("aria-selected", String(dateString === selectedDate)); if (dateString === todayDate) action.setAttribute("aria-current", "date");
        if (date.getUTCMonth() + 1 !== viewMonth) action.classList.add("datetime-day-adjacent");
        if (!action.disabled) { firstAllowed ??= action; if (dateString === activeDate) activeButton = action; }
        cell.append(action); row.append(cell);
      }
      body.append(row);
    }
    const focused = activeButton ?? firstAllowed; if (focused) focused.tabIndex = 0;
    grid.append(head, body);
    grid.addEventListener("keydown", event => safely(() => {
      if (!(event.target instanceof HTMLButtonElement) || !event.target.dataset["date"] || event.altKey || event.ctrlKey || event.metaKey) return;
      const date = dateFromIso(event.target.dataset["date"]), year = date.getUTCFullYear(), month = date.getUTCMonth() + 1, day = date.getUTCDate();
      let next: Date | undefined;
      switch (event.key) {
        case "ArrowLeft": next = calendarDate(year, month, day - 1); break;
        case "ArrowRight": next = calendarDate(year, month, day + 1); break;
        case "ArrowUp": next = calendarDate(year, month, day - 7); break;
        case "ArrowDown": next = calendarDate(year, month, day + 7); break;
        case "Home": next = calendarDate(year, month, day - (date.getUTCDay() + 6) % 7); break;
        case "End": next = calendarDate(year, month, day + 6 - (date.getUTCDay() + 6) % 7); break;
        case "PageUp": case "PageDown": {
          const target = calendarDate(year, month + (event.key === "PageDown" ? 1 : -1) * (event.shiftKey ? 12 : 1), 1);
          const maxDay = calendarDate(target.getUTCFullYear(), target.getUTCMonth() + 2, 0).getUTCDate();
          next = calendarDate(target.getUTCFullYear(), target.getUTCMonth() + 1, Math.min(day, maxDay)); break;
        }
        case "Enter": case " ": event.preventDefault(); event.target.click(); return;
      }
      if (!next || next.getUTCFullYear() < 1 || next.getUTCFullYear() > 9999) return;
      event.preventDefault(); activeDate = isoDate(next); viewYear = next.getUTCFullYear(); viewMonth = next.getUTCMonth() + 1; renderCalendar(true);
    }));
    const actions = el("div", undefined, "datetime-calendar-actions");
    const todayButton = button("Today", () => safely(() => { const current = localToday(locale); changeDate(isoDate(calendarDate(current.year, current.month, current.day))); })); todayButton.dataset["editorAction"] = "today";
    todayButton.disabled = !allowedDay(todayDate);
    const clearButton = button("Clear", () => safely(() => apply(false, true))); clearButton.dataset["editorAction"] = "clear";
    const cancel = button("Cancel", () => safely(() => { restore(); close("escape", true); })); cancel.dataset["editorAction"] = "cancel";
    actions.append(todayButton, clearButton, cancel);
    if (kind === "datetime" && showTime) { const accept = button("Apply", () => safely(() => parseInput(true)), "primary"); accept.dataset["editorAction"] = "apply"; actions.append(accept); }
    calendar.replaceChildren(header, grid, ...(time ? [time.node] : []), actions);
    if (focusDate) focused?.focus();
  };
  const open = (focusDate: boolean): void => {
    if (!enabled()) return;
    if (overlay) { if (focusDate) calendar?.querySelector<HTMLElement>('.datetime-day[tabindex="0"],.datetime-period[tabindex="0"]')?.focus(); return; }
    const parsed = parse(input.value);
    if (!parsed.ok) { restore(); invalid(parsed.error); return; }
    draft = parsed.value; const selected = selectedParts(); viewYear = selected.year; viewMonth = selected.month; activeDate = isoDate(calendarDate(selected.year, selected.month, selected.day));
    calendar = el("div", undefined, "datetime-calendar");
    try {
      if (kind === "datetime" && showTime) time = createTimeEditor({ name: `${options.name}_time`, label: "Time", value: canonicalTime(selected), locale, popovers: options.popovers, embedded: true, showSeconds,
        ...(options.rounding === undefined ? {} : { rounding: options.rounding }), ...(options.signal ? { signal: options.signal } : {}),
        onApply: value => {
          if (!enabled()) return; if (value === false) { invalid(new ClientError("validation", "Enter a valid time.")); return; }
          const current = selectedParts(); updateDraft({ ...current, hour: Number(value.slice(0, 2)), minute: Number(value.slice(3, 5)), second: Number(value.slice(6, 8)) });
        }, onInvalid: failure => invalid(failure), report: options.report });
      renderCalendar(false);
      overlay = options.popovers.open(area, calendar, { role: "dialog", presentation: window.innerWidth <= 700 ? "sheet" : "anchored", ...(options.signal ? { signal: options.signal } : {}), onClose: reason => {
        overlay = undefined;
        if (reason === "outside") safely(() => {
          let pending: ParseResult | undefined;
          try {
            // Complete main-text entry owns the whole value. If both surfaces have pending
            // edits, it takes precedence over the composed picker draft, including clearing.
            if (input.value !== display(draft)) pending = parse(input.value);
            else if (time) {
              const current = selectedParts(), parsedTime = parseTimeValue(time.input.value, locale, { previous: canonicalTime(current), showSeconds, ...(options.rounding === undefined ? {} : { rounding: options.rounding }) });
              if (!parsedTime.ok) pending = parsedTime;
              else if (parsedTime.value === false || time.input.validity.badInput) pending = { ok: false, error: new ClientError("validation", "Enter a valid time.") };
              else pending = valueFromParts(kind, { ...current, hour: Number(parsedTime.value.slice(0, 2)), minute: Number(parsedTime.value.slice(3, 5)), second: Number(parsedTime.value.slice(6, 8)) }, locale, represented);
            }
          } finally { releaseCalendar(); }
          if (pending && !pending.ok) { restore(); invalid(pending.error); return; }
          if (pending?.ok) { draft = pending.value; input.value = display(draft); }
          parseInput(false);
        });
        else { releaseCalendar(); if (reason !== "explicit") restore(); }
        if (reason === "escape" && enabled() && input.isConnected) input.focus();
      } });
      overlay.node.setAttribute("aria-label", `Choose ${options.label}`); input.setAttribute("aria-controls", overlay.node.id); opener?.setAttribute("aria-expanded", "true");
      if (focusDate) calendar?.querySelector<HTMLElement>('.datetime-day[tabindex="0"],.datetime-period[tabindex="0"]')?.focus();
    } catch (failure) { releaseCalendar(); throw failure; }
  };
  const opener = options.readonly ? undefined : button("", () => safely(() => open(true)), "datetime-open");
  if (opener) { opener.append(icon(kind === "date" ? "calendar" : "clock")); opener.setAttribute("aria-label", `Choose ${options.label}`); opener.setAttribute("aria-haspopup", "dialog"); opener.setAttribute("aria-expanded", "false"); opener.disabled = dead; area.append(opener); }
  const onInput = (): void => safely(() => {
    if (!enabled()) return;
    const parsed = parse(input.value);
    if (!parsed.ok) invalid(parsed.error);
    else if (validValue(parsed.value)) clearError();
  });
  const onChange = (): void => safely(() => parseInput(false));
  const onClick = (): void => safely(() => open(false));
  const onKey = (event: KeyboardEvent): void => safely(() => {
    if (!enabled() || event.ctrlKey || event.metaKey) return;
    if (event.key === "Escape") { event.preventDefault(); restore(); close("escape", true); }
    else if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); parseInput(true); }
    else if (event.key === "Tab") { parseInput(false); close(); }
    else if (event.key === "ArrowDown") { event.preventDefault(); open(true); }
  });
  const onBlur = (event: FocusEvent): void => {
    const next = event.relatedTarget; if (next instanceof Node && (node.contains(next) || overlay?.node.contains(next))) return;
    queueMicrotask(() => safely(() => { if (enabled() && !node.contains(document.activeElement) && !overlay?.node.contains(document.activeElement)) parseInput(false); }));
  };
  const onResize = (): void => safely(() => { if (overlay && overlay.node.dataset["presentation"] !== (window.innerWidth <= 700 ? "sheet" : "anchored")) { close(); open(false); } });
  input.value = display(represented); refreshWarning(represented);
  const dispose = (): void => {
    if (dead) return; dead = true;
    try { close("dispose"); } finally {
      options.signal?.removeEventListener("abort", dispose); window.removeEventListener("resize", onResize);
      input.removeEventListener("input", onInput); input.removeEventListener("change", onChange); input.removeEventListener("click", onClick); input.removeEventListener("keydown", onKey); input.removeEventListener("blur", onBlur);
    }
  };
  if (!dead) { input.addEventListener("input", onInput); input.addEventListener("change", onChange); input.addEventListener("click", onClick); input.addEventListener("keydown", onKey); input.addEventListener("blur", onBlur); window.addEventListener("resize", onResize); options.signal?.addEventListener("abort", dispose, { once: true }); }
  return { node, input, setValue: value => { if (dead) throw new ClientError("validation", "Date/time editor is disposed."); partsFromValue(kind, value, locale); represented = value; restore(); close(); }, dispose };
}

/** Paired endpoints apply only a complete valid tuple through the owner's callback. */
export function createDateTimeRangeEditor(options: DateTimeRangeEditorOptions): DateTimeRangeEditorHandle {
  if (!options || !Array.isArray(options.names) || options.names.length !== 2 || !Array.isArray(options.labels) || options.labels.length !== 2 || !Array.isArray(options.value) || options.value.length !== 2 || typeof options.onApply !== "function" || options.required !== undefined && (!Array.isArray(options.required) || options.required.length !== 2 || options.required.some(value => typeof value !== "boolean"))) throw new ClientError("validation", "Invalid date/time range options.");
  if (options.readonlyEndpoints !== undefined && (!Array.isArray(options.readonlyEndpoints) || options.readonlyEndpoints.length !== 2 || options.readonlyEndpoints.some(value => typeof value !== "boolean"))) throw new ClientError("validation", "Invalid date/time range readonly endpoints.");
  const { names, labels, value, required, readonlyEndpoints, onApply, ...shared } = options;
  let draft: [EditorValue, EditorValue] = [value[0], value[1]], represented: [EditorValue, EditorValue] = [...draft], dead = false;
  const node = el("div", undefined, "datetime-range-editor");
  const error = el("small", undefined, "editor-validation"); error.hidden = true; error.setAttribute("role", "status");
  const rangeErrors = new Map<0 | 1, string>();
  const invalid = (index: 0 | 1, failure: ClientError): void => {
    rangeErrors.set(index, failure.message);
    const input = index === 0 ? start.input : end.input; input.setCustomValidity(failure.message); input.setAttribute("aria-invalid", "true");
    error.textContent = failure.message; error.hidden = false; options.onInvalid?.(failure);
  };
  const apply = (index: 0 | 1, next: EditorValue): void => {
    if (dead || options.readonly || readonlyEndpoints?.[index] || options.signal?.aborted) return;
    draft[index] = next;
    for (const [endpoint, message] of rangeErrors) {
      const input = endpoint === 0 ? start.input : end.input;
      if (input.validationMessage === message) { input.setCustomValidity(""); input.removeAttribute("aria-invalid"); }
    }
    rangeErrors.clear();
    for (const endpoint of [0, 1] as const) if (required?.[endpoint] && draft[endpoint] === false) { invalid(endpoint, new ClientError("validation", `${labels[endpoint]} is required.`)); return; }
    if (draft[0] !== false && draft[1] !== false && draft[0] > draft[1]) { invalid(index, new ClientError("validation", "Range start must not be after range end.")); return; }
    if (!start.input.checkValidity() || !end.input.checkValidity()) return;
    start.input.setCustomValidity(""); end.input.setCustomValidity(""); start.input.removeAttribute("aria-invalid"); end.input.removeAttribute("aria-invalid"); error.hidden = true; error.textContent = "";
    if (draft[0] === represented[0] && draft[1] === represented[1]) return;
    const previous = represented; represented = [...draft];
    try { onApply([...represented]); } catch (failure) {
      represented = previous; draft = [...previous];
      if (!dead) { start.setValue(previous[0]); end.setValue(previous[1]); }
      throw failure;
    }
  };
  const start = createDateTimeEditor({ ...shared, notifyUnchanged: true, name: names[0], label: labels[0], value: draft[0], ...(required ? { required: required[0] } : {}), ...(readonlyEndpoints ? { readonly: readonlyEndpoints[0] || options.readonly } : {}), onApply: next => apply(0, next) });
  let end: DateTimeEditorHandle;
  try { end = createDateTimeEditor({ ...shared, notifyUnchanged: true, name: names[1], label: labels[1], value: draft[1], ...(required ? { required: required[1] } : {}), ...(readonlyEndpoints ? { readonly: readonlyEndpoints[1] || options.readonly } : {}), onApply: next => apply(1, next) }); }
  catch (failure) { start.dispose(); throw failure; }
  node.append(start.node, end.node, error);
  return { node, inputs: [start.input, end.input], setValue: next => {
    if (dead || options.signal?.aborted) throw new ClientError("validation", "Date/time range editor is disposed.");
    if (!Array.isArray(next) || next.length !== 2) throw new ClientError("validation", "Date/time range value must contain two endpoints.");
    assertEditorValue(options.kind, next[0]); assertEditorValue(options.kind, next[1]); draft = [next[0], next[1]]; represented = [...draft]; start.setValue(next[0]); end.setValue(next[1]);
  }, dispose: () => {
    if (dead) return; dead = true; const errors: unknown[] = [];
    for (const endpoint of [start, end]) { try { endpoint.dispose(); } catch (failure) { errors.push(failure); } }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, "Date/time range cleanup failed.");
  } };
}

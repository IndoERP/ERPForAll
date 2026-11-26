/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The calendar view's DOM: toolbar, the day, week, month and year grids, the side panel
// with its mini calendar and filters, and the event popover. The model (`calendar.ts`) owns every read; this file draws its
// state and asks it for changes. Creating, moving and resizing events comes with.
import { calendarEditor } from "./calendar-edit.js";
import { DateTime, Interval } from "luxon";
import { CALENDAR_SCALES, computeRange, eventsOn, layoutDay, step, type CalendarEvent, type CalendarModel } from "./calendar.js";
import { formatEditorValue, type EditorLocale } from "./datetime-values.js";
import { button, el } from "./dom.js";
import { displayValue } from "./fields.js";
import { icon } from "./icons.js";
import type { CalendarScale } from "./metadata.js";
import type { PopoverHandle, PopoverService } from "./popover.js";

const SCALE_LABELS: Record<CalendarScale, string> = { day: "Day", week: "Week", month: "Month", year: "Year" };
/** First hour shown when today is not in the range (a working morning); with today, two hours before now, as the reference does. */
const MORNING_HOUR = 8;

export interface CalendarView {
  element: HTMLElement;
  /** Draw the model's present state again. */
  render(): void;
  /** Read the range again (the search changed) and draw it while `current` holds. */
  reload(current: () => boolean): Promise<void>;
  dispose(): void;
}
export interface CalendarViewOptions {
  model: CalendarModel;
  locale: EditorLocale;
  popovers: PopoverService;
  /** The application's job runner: it reports a failed read. */
  run: (job: () => Promise<void>) => void;
  /** Open the event's record (the form view). */
  open: (event: CalendarEvent) => Promise<void>;
  /** Ask and delete the event's record; true when it was deleted. Absent: no Delete button. */
  create?: (values: Record<string, string>, title?: string) => Promise<void>;
  update?: (event: CalendarEvent, values: Record<string, string>) => Promise<void>;
  access?: (event: CalendarEvent) => Promise<{ edit: boolean; remove: boolean }>;
  remove?: (event: CalendarEvent) => Promise<boolean>;
  /** Called after the scale or the date changed and the range is drawn (the application keeps the address current). */
  changed: () => void;
  /** The first read was refused (the event bound): the view opens with the error state and this message, which names
   *  the bound and its setting (the application cannot report it in a dialog while the page is still being committed). */
  failed?: string;
}

export function mountCalendar(options: CalendarViewOptions): CalendarView {
  const { model, locale, popovers, run } = options;
  const root = model.config.root, lang = locale.lang.replaceAll("_", "-");
  const local = (value: DateTime): DateTime => value.setLocale(lang);
  const element = el("div", undefined, "calendar-view");
  let disposed = false, failed = options.failed !== undefined, failure = options.failed, sideOpen = window.matchMedia?.("(min-width: 768px)")?.matches ?? true;
  let miniMonth = model.state.date.startOf("month"), scrolledFor = "", popover: PopoverHandle | null = null;
  const sideId = `calendar-side-${Math.random().toString(36).slice(2)}`;

  const act = (job: () => Promise<void>): void => run(async () => {
    try { await job(); failed = false; failure = undefined; options.changed(); }
    catch (error) { failed = true; failure = undefined; throw error; } // a later failure is reported by the job runner
    finally { if (!disposed) render(); }
  });
  const go = (next: { scale?: CalendarScale; date?: DateTime }): void => {
    if (next.date) miniMonth = next.date.startOf("month");
    act(() => model.go(next));
  };
  /** A day opens in the shortest scale offered (a click on a month or year day). */
  const openDay = (day: DateTime): void => {
    const scale = (["day", "week", "month"] as const).find(item => root.scales.includes(item));
    go(scale ? { scale, date: day } : { date: day });
  };

  // ---- formatting ----
  const dayTitle = (day: DateTime): string => local(day).toLocaleString({ weekday: "long", day: "numeric", month: "long", year: "numeric" });
  const time = (value: DateTime): string => local(value).toLocaleString(DateTime.TIME_SIMPLE);
  const rangeLabel = (): string => {
    const { scale, date } = model.state, range = computeRange(scale, date, false);
    if (scale === "day") return dayTitle(date);
    if (scale === "year") return String(date.year);
    if (scale === "month") return local(date).toLocaleString({ month: "long", year: "numeric" });
    return Interval.fromDateTimes(local(range.start), local(range.end.minus({ days: 1 }))).toLocaleString(DateTime.DATE_MED);
  };
  const when = (event: CalendarEvent): string => {
    const last = event.end.minus({ days: 1 });
    if (event.allDay) {
      if (root.hideDate) return "All day";
      return last > event.start ? `${dayTitle(event.start)} – ${dayTitle(last)}` : dayTitle(event.start);
    }
    const parts = [root.hideDate ? "" : dayTitle(event.start), root.hideTime ? "" : `${time(event.start)} – ${time(event.end)}`];
    return parts.filter(Boolean).join(", ") || dayTitle(event.start);
  };

  // ---- event chips and popover ----
  const chip = (event: CalendarEvent, compact: boolean): HTMLButtonElement => {
    const node = el("button", undefined, `calendar-event${event.allDay ? " is-all-day" : ""}`);
    node.type = "button";
    if (event.color) node.dataset["color"] = String(event.color);
    const label = el("span", event.title, "calendar-event-title");
    if (!event.allDay && !root.hideTime && compact) node.append(el("span", time(event.start), "calendar-event-time"));
    node.append(label);
    if (!event.allDay && !root.hideTime && !compact) node.append(el("span", `${time(event.start)} – ${time(event.end)}`, "calendar-event-time"));
    node.setAttribute("aria-label", `${event.title}, ${when(event)}`);
    node.title = `${event.title}\n${when(event)}`;
    node.addEventListener("click", () => openPopover(event, node));
    return node;
  };
  const closePopover = (): void => { const open = popover; popover = null; open?.close(); };
  const openPopover = (event: CalendarEvent, anchor: HTMLElement): void => {
    closePopover();
    anchor.focus(); // the popover gives focus back to what had it when it opened
    const body = el("div", undefined, "calendar-popover");
    const head = el("div", undefined, "calendar-popover-head");
    const heading = el("h3", event.title, "calendar-popover-title");
    if (event.color) heading.dataset["color"] = String(event.color);
    const close = button("", () => closePopover(), "calendar-icon-button");
    close.append(icon("x")); close.setAttribute("aria-label", "Close");
    head.append(heading, close);
    const details = el("dl", undefined, "calendar-popover-fields");
    const row = (label: string, value: string): void => { if (value) details.append(el("dt", label), el("dd", value)); };
    row("When", when(event));
    for (const item of model.config.fields) {
      if (item.invisible) continue;
      const raw = event.record[item.field.name];
      const value = item.field.kind === "date" || item.field.kind === "datetime"
        ? typeof raw === "string" ? formatEditorValue(item.field.kind, raw, locale) : ""
        : displayValue(item.field, raw, lang);
      row(item.label, value);
    }
    const actions = el("div", undefined, "calendar-popover-actions");
    const record = button(root.edit && !options.access ? "Edit" : "Open", () => { closePopover(); run(() => options.open(event)); }, "erp-button erp-button-primary");
    actions.append(record);
    const permitted = (edit: boolean, unlink: boolean): void => {
      record.textContent = edit ? "Edit" : "Open";
      if (options.update && edit && (!root.dateStart.readonly || (root.dateStop && !root.dateStop.readonly))) {
        const change = button("Change dates", () => {
          const form = calendarEditor(root, locale, event.start, event, async values => {
            await options.update!(event, values);
            closePopover(); await model.load(); render();
          }, run);
          actions.replaceChildren(form); form.querySelector("input")?.focus();
        }, "erp-button");
        actions.append(change);
      }
      const remove = options.remove;
      if (remove && unlink) {
        const del = button("Delete", () => {
          closePopover();
          act(async () => { if (await remove(event)) await model.load(); });
        }, "erp-button calendar-delete");
        del.prepend(icon("trash"));
        actions.append(del);
      }
    };
    if (options.access && (root.edit || root.delete)) {
      run(async () => {
        const access = await options.access!(event);
        if (!disposed && body.isConnected) permitted(root.edit && access.edit, root.delete && access.remove);
      });
    } else permitted(root.edit, root.delete);
    body.append(head, details, actions);
    anchor.setAttribute("aria-haspopup", "dialog");
    popover = popovers.open(anchor, body, { placement: "right", role: "dialog", dismissOnOutside: true, restoreFocus: true, onClose: () => { popover = null; } });
    (actions.firstElementChild as HTMLElement | null)?.focus();
  };
  const moreList = (day: DateTime, events: readonly CalendarEvent[], anchor: HTMLElement): void => {
    closePopover();
    anchor.focus();
    const body = el("div", undefined, "calendar-popover calendar-more-list");
    body.append(el("h3", dayTitle(day), "calendar-popover-title"));
    const list = el("div", undefined, "calendar-more-events");
    for (const event of events) {
      const item = chip(event, true);
      item.addEventListener("click", eventClick => { eventClick.stopImmediatePropagation(); closePopover(); openPopover(event, anchor); }, { capture: true });
      list.append(item);
    }
    body.append(list);
    popover = popovers.open(anchor, body, { placement: "bottom", role: "dialog", dismissOnOutside: true, restoreFocus: true, onClose: () => { popover = null; } });
    (list.firstElementChild as HTMLElement | null)?.focus();
  };

  // ---- day buttons (month cells, year and mini calendar) with arrow-key movement ----
  const dayButton = (day: DateTime, label: string, onClick: () => void, className: string): HTMLButtonElement => {
    const node = button(label, onClick, className);
    node.dataset["day"] = day.toISODate()!;
    node.tabIndex = -1;
    return node;
  };
  /** One tab stop per grid: the focused day moves with the arrow keys (a day, or a week up and down). */
  const roving = (grid: HTMLElement, focusDay: DateTime): void => {
    const days = [...grid.querySelectorAll<HTMLButtonElement>("button[data-day]")];
    const current = days.find(item => item.dataset["day"] === focusDay.toISODate()) ?? days[0];
    if (current) current.tabIndex = 0;
    grid.addEventListener("keydown", event => {
      const target = event.target;
      if (!(target instanceof HTMLButtonElement) || target.dataset["day"] === undefined) return;
      const delta = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7 }[event.key];
      if (delta === undefined) return;
      const wanted = DateTime.fromISO(target.dataset["day"]!).plus({ days: delta }).toISODate();
      const next = grid.querySelector<HTMLButtonElement>(`button[data-day="${wanted}"]`);
      event.preventDefault();
      if (!next) return;
      target.tabIndex = -1; next.tabIndex = 0; next.focus();
    });
  };
  /** Weekday names: column headers of the month grid; elsewhere hidden from assistive technology (each day button names its day). */
  const weekdayHeads = (start: DateTime, format: "short" | "narrow", header: boolean): HTMLElement[] =>
    Array.from({ length: 7 }, (_, index) => {
      const head = el("div", local(start.plus({ days: index })).toLocaleString({ weekday: format }), "calendar-weekday");
      if (header) head.setAttribute("role", "columnheader"); else head.setAttribute("aria-hidden", "true");
      return head;
    });

  // ---- toolbar ----
  const toolbar = (): HTMLElement => {
    const bar = el("div", undefined, "calendar-toolbar");
    bar.setAttribute("role", "toolbar"); bar.setAttribute("aria-label", "Calendar navigation");
    const today = button("Today", () => go({ date: model.today() }), "erp-button");
    const previous = button("", () => go({ date: step(model.state.scale, model.state.date, -1) }), "erp-button calendar-icon-button");
    previous.append(icon("chevron-left")); previous.setAttribute("aria-label", `Previous ${model.state.scale}`);
    const next = button("", () => go({ date: step(model.state.scale, model.state.date, 1) }), "erp-button calendar-icon-button");
    next.append(icon("chevron-right")); next.setAttribute("aria-label", `Next ${model.state.scale}`);
    const label = el("h2", rangeLabel(), "calendar-range-label");
    label.setAttribute("aria-live", "polite");
    bar.append(today, previous, next, label);
    if (root.create && options.create && !root.dateStart.readonly) {
      const create = button("New event", () => {
        closePopover();
        const form = calendarEditor(root, locale, model.state.date.startOf("day").set({ hour: root.dateStart.kind === "date" ? 0 : 9 }), undefined, async (values, name) => {
          await options.create!(values, name);
          closePopover(); if (!disposed) { await model.load(); render(); }
        }, run);
        popover = popovers.open(create, form, { placement: "bottom", role: "dialog", restoreFocus: true, onClose: () => { popover = null; } });
        form.querySelector("input")?.focus();
      }, "erp-button");
      bar.prepend(create);
    }
    if (model.state.scale === "day" || model.state.scale === "week") {
      const week = model.range.start.weekNumber;
      const badge = el("span", `Week ${week}`, "calendar-week-number");
      bar.append(badge);
    }
    const end = el("div", undefined, "calendar-toolbar-end");
    if (root.scales.length > 1) {
      const group = el("div", undefined, "calendar-scales");
      group.setAttribute("role", "group"); group.setAttribute("aria-label", "Scale");
      for (const scale of CALENDAR_SCALES.filter(item => root.scales.includes(item))) {
        const choice = button(SCALE_LABELS[scale], () => go({ scale }), "calendar-scale");
        choice.setAttribute("aria-pressed", String(scale === model.state.scale));
        group.append(choice);
      }
      end.append(group);
    }
    const toggle = button("", () => { sideOpen = !sideOpen; render(); }, "erp-button calendar-icon-button calendar-side-toggle");
    toggle.append(icon("sliders-horizontal"));
    toggle.setAttribute("aria-label", sideOpen ? "Hide the side panel" : "Show the side panel");
    toggle.setAttribute("aria-expanded", String(sideOpen)); toggle.setAttribute("aria-controls", sideId);
    end.append(toggle);
    bar.append(end);
    return bar;
  };

  // ---- grids ----
  const timeGrid = (days: DateTime[], events: readonly CalendarEvent[]): HTMLElement => {
    const grid = el("div", undefined, "calendar-time-grid");
    grid.style.setProperty("--calendar-days", String(days.length));
    grid.setAttribute("role", "group"); grid.setAttribute("aria-label", rangeLabel());
    const today = model.today();
    const heads = el("div", undefined, "calendar-day-heads");
    heads.setAttribute("aria-hidden", "true"); // each day column names its day
    heads.append(el("div", undefined, "calendar-gutter"));
    for (const day of days) {
      const head = el("div", undefined, `calendar-day-head${day.hasSame(today, "day") ? " is-today" : ""}`);
      head.append(el("span", local(day).toLocaleString({ weekday: "short" }), "calendar-day-head-name"), el("span", String(day.day), "calendar-day-head-number"));
      heads.append(head);
    }
    // All-day row: an event that covers several days spans their columns.
    const allDay = el("div", undefined, "calendar-all-day");
    allDay.setAttribute("role", "group"); allDay.setAttribute("aria-label", "All-day events");
    const allDayLabel = el("div", "All day", "calendar-gutter calendar-all-day-label");
    allDay.append(allDayLabel);
    const lane = el("div", undefined, "calendar-all-day-lane");
    const first = days[0]!, end = days[days.length - 1]!.plus({ days: 1 });
    // Today's column stays marked through the all-day row too, as in the time grid below it.
    const todayIndex = days.findIndex(day => day.hasSame(today, "day"));
    if (todayIndex >= 0) {
      const mark = el("div", undefined, "calendar-all-day-today");
      mark.setAttribute("aria-hidden", "true");
      mark.style.setProperty("--calendar-today", String(todayIndex));
      lane.append(mark);
    }
    for (const event of events.filter(item => item.allDay && item.start < end && item.end > first)) {
      const from = Math.max(0, Math.round(event.start.diff(first, "days").days));
      const to = Math.min(days.length, Math.round(event.end.diff(first, "days").days));
      const node = chip(event, true);
      node.style.gridColumn = `${from + 1} / ${to + 1}`;
      lane.append(node);
    }
    allDay.append(lane);
    const body = el("div", undefined, "calendar-time-body");
    const gutter = el("div", undefined, "calendar-gutter calendar-hours");
    for (let hour = 0; hour < 24; hour++) gutter.append(el("div", hour ? time(first.set({ hour })) : "", "calendar-hour-label"));
    body.append(gutter);
    for (const day of days) {
      const column = el("div", undefined, `calendar-day-column${day.hasSame(today, "day") ? " is-today" : ""}`);
      column.setAttribute("role", "group"); column.setAttribute("aria-label", dayTitle(day));
      for (const placed of layoutDay(events, day)) {
        const node = chip(placed.event, placed.height < 45);
        node.classList.add("is-timed");
        node.style.top = `calc(var(--calendar-hour) * ${placed.top / 60})`;
        node.style.height = `calc(var(--calendar-hour) * ${placed.height / 60})`;
        node.style.insetInlineStart = `${(placed.column / placed.columns) * 100}%`;
        node.style.width = `${100 / placed.columns}%`;
        column.append(node);
      }
      if (day.hasSame(today, "day")) {
        const now = DateTime.now().setZone(model.zone), line = el("div", undefined, "calendar-now");
        line.style.top = `calc(var(--calendar-hour) * ${(now.hour * 60 + now.minute) / 60})`;
        line.setAttribute("aria-hidden", "true");
        column.append(line);
      }
      body.append(column);
    }
    grid.append(heads, allDay, body);
    const key = `${model.state.scale}:${model.state.date.toISODate()}`;
    if (scrolledFor !== key) {
      scrolledFor = key;
      const showsToday = days.some(day => day.hasSame(today, "day"));
      const hour = showsToday ? Math.max(0, DateTime.now().setZone(model.zone).hour - 2) : MORNING_HOUR;
      window.requestAnimationFrame?.(() => { if (body.isConnected) body.scrollTop = body.scrollHeight * hour / 24; });
    }
    return grid;
  };
  const monthGrid = (events: readonly CalendarEvent[]): HTMLElement => {
    const range = model.range, month = model.state.date.month, today = model.today();
    const grid = el("div", undefined, "calendar-month-grid");
    grid.setAttribute("role", "grid"); grid.setAttribute("aria-label", rangeLabel());
    const heads = el("div", undefined, "calendar-month-row calendar-month-heads");
    heads.setAttribute("role", "row");
    const weekHead = el("div", undefined, "calendar-week-cell");
    weekHead.setAttribute("role", "columnheader"); weekHead.setAttribute("aria-label", "Week");
    heads.append(weekHead, ...weekdayHeads(range.start.minus({ days: range.start.weekday - 1 }), "short", true));
    grid.append(heads);
    // Without the month overflow the range is the month itself: the first and last weeks are padded with empty cells.
    let day = range.start.minus({ days: range.start.weekday - 1 });
    while (day < range.end) {
      const row = el("div", undefined, "calendar-month-row");
      row.setAttribute("role", "row");
      const week = el("div", String(day.weekNumber), "calendar-week-cell");
      week.setAttribute("role", "rowheader"); week.setAttribute("aria-label", `Week ${day.weekNumber}`);
      row.append(week);
      const weekStart = day, weekEnd = day.plus({ weeks: 1 });
      const lanes: DateTime[] = [];
      const spans = events.filter(event => event.start < weekEnd && event.end > weekStart && event.start < range.end && event.end > range.start)
        .sort((a, b) => a.start.startOf("day").toMillis() - b.start.startOf("day").toMillis() || Number(b.allDay) - Number(a.allDay) || a.start.toMillis() - b.start.toMillis() || a.id - b.id)
        .map(event => {
          const start = DateTime.max(weekStart, range.start, event.start.startOf("day"));
          const end = DateTime.min(weekEnd, range.end, event.end.minus({ milliseconds: 1 }).startOf("day").plus({ days: 1 }));
          let lane = lanes.findIndex(until => until <= start);
          if (lane === -1) lane = lanes.length;
          lanes[lane] = end;
          return { event, start, end, lane };
        });
      for (let index = 0; index < 7; index++, day = day.plus({ days: 1 })) {
        const cell = el("div", undefined, "calendar-month-cell");
        cell.setAttribute("role", "gridcell");
        if (day < range.start || day >= range.end) { cell.classList.add("is-padding"); row.append(cell); continue; }
        if (day.month !== month) cell.classList.add("is-other-month");
        if (day.hasSame(today, "day")) cell.classList.add("is-today");
        const number = dayButton(day, String(day.day), () => openDay(cellDay), "calendar-month-day");
        const cellDay = day;
        number.setAttribute("aria-label", dayTitle(day));
        cell.append(number);
        const onDay = spans.filter(span => span.start <= day && span.end > day);
        const shown = onDay.filter(span => span.lane < root.eventLimit);
        for (const span of shown) {
          if (!span.start.equals(day)) continue;
          const bar = chip(span.event, true);
          const width = Math.round(span.end.diff(day, "days").days);
          bar.style.setProperty("--span-days", String(width));
          bar.style.setProperty("--span-lane", String(span.lane));
          bar.classList.add("calendar-month-event");
          if (span.event.start < span.start) bar.classList.add("continues-before");
          if (span.event.end > span.end) bar.classList.add("continues-after");
          cell.append(bar);
        }
        cell.style.setProperty("--event-lanes", String(shown.length ? Math.max(...shown.map(span => span.lane)) + 1 : 0));
        if (onDay.length > shown.length) {
          const more = button(`+${onDay.length - shown.length} more`, () => moreList(cellDay, onDay.map(span => span.event), more), "calendar-more");
          cell.append(more);
        }
        row.append(cell);
      }
      grid.append(row);
    }
    roving(grid, model.state.date);
    return grid;
  };
  const yearGrid = (events: readonly CalendarEvent[]): HTMLElement => {
    const grid = el("div", undefined, "calendar-year-grid"), today = model.today();
    grid.setAttribute("aria-label", rangeLabel());
    for (let index = 0; index < 12; index++) {
      const first = model.range.start.plus({ months: index });
      const block = el("section", undefined, "calendar-year-month");
      const title = el("h3", local(first).toLocaleString({ month: "long" }), "calendar-year-title");
      block.append(title);
      const days = el("div", undefined, "calendar-mini-days");
      days.setAttribute("role", "group"); days.setAttribute("aria-label", local(first).toLocaleString({ month: "long", year: "numeric" }));
      days.append(...weekdayHeads(first.minus({ days: first.weekday - 1 }), "narrow", false));
      for (let pad = 1; pad < first.weekday; pad++) days.append(el("span", undefined, "calendar-mini-pad"));
      for (let day = first; day.month === first.month; day = day.plus({ days: 1 })) {
        const count = eventsOn(events, day).length, current = day;
        const node = dayButton(day, String(day.day), () => openDay(current), "calendar-mini-day");
        if (count) { node.classList.add("has-events"); node.dataset["count"] = String(count); }
        if (day.hasSame(today, "day")) node.classList.add("is-today");
        node.setAttribute("aria-label", `${dayTitle(day)}${count ? `, ${count} ${count === 1 ? "event" : "events"}` : ""}`);
        days.append(node);
      }
      block.append(days);
      grid.append(block);
    }
    roving(grid, model.state.date);
    return grid;
  };

  // ---- side panel ----
  const miniCalendar = (): HTMLElement => {
    const wrap = el("section", undefined, "calendar-mini");
    wrap.setAttribute("aria-label", "Date picker");
    const head = el("div", undefined, "calendar-mini-head");
    const label = el("span", local(miniMonth).toLocaleString({ month: "long", year: "numeric" }), "calendar-mini-label");
    const previous = button("", () => { miniMonth = miniMonth.minus({ months: 1 }); render(); focusMini("previous"); }, "calendar-icon-button");
    previous.append(icon("chevron-left")); previous.setAttribute("aria-label", "Previous month"); previous.dataset["mini"] = "previous";
    const next = button("", () => { miniMonth = miniMonth.plus({ months: 1 }); render(); focusMini("next"); }, "calendar-icon-button");
    next.append(icon("chevron-right")); next.setAttribute("aria-label", "Next month"); next.dataset["mini"] = "next";
    head.append(label, previous, next);
    const days = el("div", undefined, "calendar-mini-days");
    days.setAttribute("role", "group"); days.setAttribute("aria-label", label.textContent ?? "");
    const start = miniMonth.minus({ days: miniMonth.weekday - 1 }), range = model.range, today = model.today();
    days.append(...weekdayHeads(start, "narrow", false));
    for (let day = start; day < start.plus({ weeks: 6 }); day = day.plus({ days: 1 })) {
      const current = day;
      const node = dayButton(day, String(day.day), () => go({ date: current }), "calendar-mini-day");
      if (day.month !== miniMonth.month) node.classList.add("is-other-month");
      if ((model.state.scale === "day" || model.state.scale === "week") && day >= range.start && day < range.end) node.classList.add("in-range");
      if (day.hasSame(model.state.date, "day")) node.setAttribute("aria-current", "date");
      if (day.hasSame(today, "day")) node.classList.add("is-today");
      node.setAttribute("aria-label", dayTitle(day));
      days.append(node);
    }
    roving(days, miniMonth.hasSame(model.state.date, "month") ? model.state.date : miniMonth);
    wrap.append(head, days);
    return wrap;
  };
  const focusMini = (which: string): void => element.querySelector<HTMLElement>(`[data-mini="${which}"]`)?.focus();
  const filters = (): HTMLElement[] => model.sections.map(section => {
    const wrap = el("section", undefined, "calendar-filter-section");
    const name = section.field.name, bodyId = `${sideId}-${name}`;
    const head = button("", () => { section.collapsed = !section.collapsed; render(); element.querySelector<HTMLElement>(`[data-section="${name}"]`)?.focus(); }, "calendar-filter-head");
    head.dataset["section"] = name;
    head.append(el("span", section.label), icon("chevron-down"));
    head.setAttribute("aria-expanded", String(!section.collapsed)); head.setAttribute("aria-controls", bodyId);
    wrap.append(head);
    const body = el("div", undefined, "calendar-filter-values");
    body.id = bodyId; body.hidden = section.collapsed;
    const checkbox = (label: string, checked: boolean, mixed: boolean, color: number, onChange: () => void, focusKey: string): HTMLElement => {
      const row = el("label", undefined, "calendar-filter-value");
      const input = el("input");
      input.type = "checkbox"; input.checked = checked; input.indeterminate = mixed; input.dataset["filter"] = focusKey;
      input.addEventListener("change", () => { onChange(); render(); [...element.querySelectorAll<HTMLInputElement>("input[data-filter]")].find(item => item.dataset["filter"] === focusKey)?.focus(); });
      const swatch = el("span", undefined, "calendar-swatch");
      if (color) swatch.dataset["color"] = String(color);
      row.append(input, swatch, el("span", label, "calendar-filter-label"));
      return row;
    };
    if (!section.values.length) body.append(el("p", "No values in this range.", "calendar-filter-empty"));
    else {
      const on = section.values.filter(value => value.active).length;
      body.append(checkbox("All", on === section.values.length, on > 0 && on < section.values.length, 0, () => model.toggleAll(name), `${name}:*`));
      for (const value of section.values) body.append(checkbox(value.label, value.active, false, value.color, () => model.toggle(name, value.key), `${name}:${value.key}`));
    }
    wrap.append(body);
    return wrap;
  });

  function render(): void {
    if (disposed) return;
    closePopover();
    const visible = model.visible();
    const main = el("div", undefined, "calendar-main");
    if (failed) {
      const status = el("p", failure ?? "The events of this range could not be loaded. Change the range or the search to try again.", "calendar-status is-error");
      status.setAttribute("role", "alert");
      main.append(status);
    } else {
      const scale = model.state.scale, range = model.range;
      if (!visible.length) {
        const status = el("p", "No events in this range.", "calendar-status");
        status.setAttribute("role", "status");
        main.append(status);
      }
      if (scale === "day" || scale === "week") {
        const days = Array.from({ length: Math.round(range.end.diff(range.start, "days").days) }, (_, index) => range.start.plus({ days: index }));
        main.append(timeGrid(days, visible));
      } else main.append(scale === "month" ? monthGrid(visible) : yearGrid(visible));
    }
    const side = el("aside", undefined, "calendar-side");
    side.id = sideId; side.hidden = !sideOpen;
    side.setAttribute("aria-label", "Calendar side panel");
    side.append(miniCalendar(), ...filters());
    const body = el("div", undefined, "calendar-body");
    body.append(main, side);
    element.replaceChildren(toolbar(), body);
  }

  render();
  return {
    element,
    render,
    reload: async current => {
      try { await model.load(); if (current()) { failed = false; failure = undefined; } }
      catch (error) { if (current()) { failed = true; failure = undefined; } throw error; }
      finally { if (current()) render(); }
    },
    dispose: () => { disposed = true; closePopover(); element.replaceChildren(); },
  };
}

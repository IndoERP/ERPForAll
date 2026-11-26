/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { DateTime } from "luxon";
import { describeViews, type View } from "../src/client/metadata.js";
import { CalendarModel, colorIndex, computeRange, decodeEvent, layoutDay, paramsFromState, rangeDomain, readCalendarView, stateFromParams, step, type CalendarConfig } from "../src/client/calendar.js";
import { mountCalendar } from "../src/client/calendar-view.js";
import { PopoverService } from "../src/client/popover.js";
import { parseRoute, routeUrl } from "../src/client/route.js";
import { ClientError } from "../src/client/transport.js";
import { auditComponent } from "../../../../web/testing/axe.js";

// ---- the arch as the server delivers it (addons/web/src/h18_calendar.rs) ----

const column = (type: string, label: string, extra: Record<string, unknown> = {}) =>
  ({ type, label, required: false, readonly: false, relation: null, relation_field: null, selection: null, ...extra });
const modelFields = {
  name: column("char", "Name"),
  start: column("datetime", "Start"), stop: column("datetime", "Stop"),
  day: column("date", "Day"), until: column("date", "Until"),
  all_day: column("boolean", "All day"),
  owner_id: column("many2one", "Owner", { relation: "res.users" }),
  state: column("selection", "State", { selection: [["draft", "Draft"], ["done", "Done"]] }),
};
const ref = (name: string) => ({ model: "t.event", name });
const field = (name: string, label: string, flags: { invisible?: boolean; filters?: boolean } = {}) =>
  ({ kind: "calendar_field", field: ref(name), label, invisible: flags.invisible ?? false, filters: flags.filters ?? false });
type Root = Record<string, unknown>;
const rootNode = (children: unknown[], extra: Root = {}): Root => Object.fromEntries(Object.entries({
  kind: "calendar", children: children.map((_, index) => index + 1), date_start: ref("start"), date_stop: ref("stop"), color: ref("owner_id"),
  mode: "week", scales: ["day", "week", "month", "year"], create: true, edit: true, delete: true, quick_create: true,
  hide_date: false, hide_time: false, month_overflow: true, event_limit: 2, ...extra,
}).filter(([, value]) => value !== undefined)); // an undefined member is an absent one
const describe = (children: unknown[], extra: Root = {}) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "t.event", fields: { "t.event": modelFields }, notices: [],
  views: [{ identity: { kind: "stored", id: 9 }, type: "calendar", schema_major: 1, renderer: "web.calendar", root: 0, nodes: [rootNode(children, extra), ...children] }],
});
const standardFields = () => [field("name", "Name"), field("owner_id", "Owner", { filters: true }), field("state", "State", { filters: true }), field("all_day", "All day", { invisible: true })];
const view = (children: unknown[] = standardFields(), extra: Root = {}): View => describeViews(describe(children, extra), "t.event", "r1").views.get("calendar")!;
const config = (children?: unknown[], extra?: Root): CalendarConfig => readCalendarView(view(children, extra));
const ZONE = "Asia/Jakarta"; // UTC+7, no daylight saving
const day = (iso: string): DateTime => DateTime.fromISO(iso, { zone: ZONE });

const disposers: (() => void)[] = [];
afterEach(() => { while (disposers.length) disposers.pop()!(); document.body.replaceChildren(); vi.useRealTimers(); });

/// (partial: the client decodes the calendar description the server lowers: root fields, scales, flags and its fields, closed members)
test("the calendar description decodes into its root and fields, and the read asks for every field it places or shows", () => {
  const calendar = view();
  const root = calendar.nodes[0]!;
  expect(root.kind).toBe("calendar");
  if (root.kind !== "calendar") return;
  expect([root.dateStart.name, root.dateStop?.name, root.allDay, root.color?.name]).toEqual(["start", "stop", null, "owner_id"]);
  expect(root.scales).toEqual(["day", "week", "month", "year"]);
  expect([root.mode, root.eventLimit, root.delete, root.monthOverflow]).toEqual(["week", 2, true, true]);
  expect(root.children.map(child => child.kind === "calendar_field" ? [child.field.name, child.label, child.invisible, child.filters] : null))
    .toEqual([["name", "Name", false, false], ["owner_id", "Owner", false, true], ["state", "State", false, true], ["all_day", "All day", true, false]]);
  expect(calendar.fields.map(item => item.name).sort()).toEqual(["all_day", "name", "owner_id", "start", "state", "stop"]);
  // Closed shapes: an unknown member, a scale the client does not know, a mode outside the scales, a stop of another type are refused.
  for (const extra of [{ bogus: 1 }, { scales: ["week", "fortnight"] }, { mode: "day", scales: ["week"] }, { date_stop: ref("day") }, { event_limit: 0 }]) {
    expect(() => view(standardFields(), extra)).toThrow(ClientError);
  }
  expect(() => describeViews({ ...describe([]), views: [{ ...describe([]).views[0], nodes: [rootNode([1]), { kind: "pivot_field", field: ref("name"), label: "Name", role: "row" }] }] }, "t.event", "r1")).toThrow(ClientError);
});

/// (the visible range of each scale: Monday-first weeks, a six-week month with the overflow, the plain month without it, the year)
test("each scale has its range in the user's zone", () => {
  const date = day("2026-11-18"); // a Wednesday
  const iso = (scale: Parameters<typeof computeRange>[0], overflow = true) => { const range = computeRange(scale, date, overflow); return [range.start.toISODate(), range.end.toISODate()]; };
  expect(iso("day")).toEqual(["2026-11-18", "2026-11-19"]);
  expect(iso("week")).toEqual(["2026-11-16", "2026-11-23"]);
  expect(iso("month")).toEqual(["2026-10-26", "2026-12-07"]);
  expect(iso("month", false)).toEqual(["2026-11-01", "2026-12-01"]);
  expect(iso("year")).toEqual(["2026-01-01", "2027-01-01"]);
  expect(computeRange("week", date, true).start.zoneName).toBe(ZONE);
  expect(step("month", date, -1).toISODate()).toBe("2026-10-18");
  expect(step("week", date, 1).toISODate()).toBe("2026-11-25");
});

/// (the range domain: starts before the end and stops after the start, a missing stop counts from the start; datetimes bound in UTC, dates as days)
test("the range domain bounds datetimes in UTC and dates as days", () => {
  const timed = config().root, range = computeRange("day", day("2026-11-18"), true);
  expect(rangeDomain(timed, range)).toEqual(["&", ["start", "<=", "2026-11-18 16:59:59"], "|", ["stop", ">=", "2026-11-17 17:00:00"], "&", ["stop", "=", false], ["start", ">=", "2026-11-17 17:00:00"]]);
  const dated = config(standardFields(), { date_start: ref("day"), date_stop: ref("until") }).root;
  expect(rangeDomain(dated, computeRange("week", day("2026-11-18"), true))).toEqual(["&", ["day", "<=", "2026-11-22"], "|", ["until", ">=", "2026-11-16"], "&", ["until", "=", false], ["day", ">=", "2026-11-16"]]);
  const startOnly = config(standardFields(), { date_stop: undefined }).root;
  expect(rangeDomain(startOnly, range)).toEqual([["start", "<=", "2026-11-18 16:59:59"], ["start", ">=", "2026-11-17 17:00:00"]]);
});

const owner = (id: number, name: string) => ({ id, display_name: name });
const records = [
  { id: 1, display_name: "Stand-up", start: "2026-11-18 02:00:00", stop: "2026-11-18 02:30:00", owner_id: owner(1, "Ana"), state: "draft", all_day: false, name: "Stand-up" },
  { id: 2, display_name: "Review", start: "2026-11-18 02:15:00", stop: "2026-11-18 03:30:00", owner_id: owner(2, "Budi"), state: "done", all_day: false, name: "Review" },
  { id: 3, display_name: "Offsite", start: "2026-11-19 01:00:00", stop: "2026-11-20 10:00:00", owner_id: false, state: "draft", all_day: true, name: "Offsite" },
  { id: 4, display_name: "Lunch", start: "2026-11-18 05:00:00", stop: false, owner_id: owner(1, "Ana"), state: "draft", all_day: false, name: "Lunch" },
];
function made(options: { source?: unknown[]; max?: number; date?: string; scale?: "day" | "week" | "month" | "year"; children?: unknown[]; extra?: Root } = {}) {
  const requests: { domain: unknown[]; limit: number }[] = [];
  const cfg = config(options.children ?? [...standardFields()], { all_day: ref("all_day"), ...options.extra });
  const model = new CalendarModel(cfg, async request => { requests.push(request); return { length: (options.source ?? records).length, records: options.source ?? records }; },
    () => [["name", "ilike", "a"]], { maxEvents: options.max ?? 50 }, ZONE, { scale: options.scale ?? "week", date: day(options.date ?? "2026-11-18") });
  return { model, requests, cfg };
}

/// (partial: one read of the range with the search domain, the range domain and one event over the bound; the read is web_search_read, not search_read)
/// (partial: the client half: one event more than the bound is asked for, and a range past it is refused by name with nothing drawn)
test("the range is read once under the search, and a range past the bound is refused naming the setting", async () => {
  const { model, requests } = made();
  await model.load();
  expect(requests).toHaveLength(1);
  expect(requests[0]!.limit).toBe(51);
  expect(requests[0]!.domain.slice(0, 1)).toEqual([["name", "ilike", "a"]]);
  expect(requests[0]!.domain).toContainEqual(["start", "<=", "2026-11-22 16:59:59"]);
  expect(model.events.map(event => event.id)).toEqual([1, 2, 3, 4]);
  const crowded = made({ max: 3 });
  await expect(crowded.model.load()).rejects.toThrow(/more than 3 events.*web\.calendar_max_events/);
  expect(crowded.model.events).toEqual([]);
  // A scale the arch does not offer is refused by name.
  await expect(made({ extra: { scales: ["week", "month"] } }).model.go({ scale: "day" })).rejects.toThrow("does not offer the day scale");
});

test("events are placed in the user's zone; an all-day event covers whole days, a timed one without a stop has a short length", () => {
  const { cfg } = made();
  const [standup, , offsite, lunch] = records.map(raw => decodeEvent(cfg.root, raw, ZONE)!);
  expect([standup!.start.toFormat("yyyy-MM-dd HH:mm"), standup!.end.toFormat("HH:mm"), standup!.allDay, standup!.color]).toEqual(["2026-11-18 09:00", "09:30", false, 1]);
  expect([offsite!.start.toISODate(), offsite!.end.toISODate(), offsite!.allDay, offsite!.color]).toEqual(["2026-11-19", "2026-11-21", true, 0]);
  expect(lunch!.end.diff(lunch!.start, "minutes").minutes).toBe(30);
  expect(() => decodeEvent(cfg.root, { ...records[0], start: "2026-11-18T02:00:00Z" }, ZONE)).toThrow(ClientError);
  expect([colorIndex(owner(12, "x")), colorIndex(11), colorIndex(false), colorIndex(-3)]).toEqual([1, 11, 0, 0]);
});

/// (dynamic filter sections built from the loaded records; an inactive value is pruned in memory, without a read, and stays off across reads)
test("filter sections come from the loaded events and prune in memory", async () => {
  const { model, requests } = made();
  await model.load();
  const owners = model.sections.find(section => section.field.name === "owner_id")!;
  expect(owners.values.map(value => [value.key, value.label, value.color, value.active])).toEqual([["1", "Ana", 1, true], ["2", "Budi", 2, true], ["false", "Not set", 0, true]]);
  expect(model.sections.find(section => section.field.name === "state")!.values.map(value => value.label)).toEqual(["Done", "Draft"]);
  model.toggle("owner_id", "1");
  expect(model.visible().map(event => event.id)).toEqual([2, 3]);
  expect(requests).toHaveLength(1);
  await model.load();
  expect(model.visible().map(event => event.id)).toEqual([2, 3]);
  model.toggleAll("owner_id"); // not all on: all come back on
  expect(model.visible()).toHaveLength(4);
  model.toggleAll("owner_id"); // all on: all go off
  expect(model.visible()).toEqual([]);
  expect(() => model.toggleAll("name")).toThrow(ClientError);
});

test("overlapping timed events share their cluster side by side; others keep the full width", () => {
  const { cfg } = made();
  const events = records.map(raw => decodeEvent(cfg.root, raw, ZONE)!);
  const placed = layoutDay(events, day("2026-11-18")).map(item => [item.event.id, item.top, item.height, item.column, item.columns]);
  expect(placed).toEqual([[1, 540, 30, 0, 2], [2, 555, 75, 1, 2], [4, 720, 30, 0, 1]]);
});

/// (the scale and the day in the page address: read back on reload, refused by name when malformed or not offered)
test("the scale and the day travel in the page address", () => {
  const { cfg, model } = made({ date: "2026-11-18", scale: "month" });
  expect(paramsFromState(model.state)).toEqual({ calendar_scale: "month", calendar_date: "2026-11-18" });
  const back = stateFromParams({ calendar_scale: "month", calendar_date: "2026-11-18" }, cfg.root, ZONE);
  expect([back.scale, back.date.toISODate(), back.date.zoneName]).toEqual(["month", "2026-11-18", ZONE]);
  expect(stateFromParams({}, cfg.root, ZONE).scale).toBe("week");
  expect(() => stateFromParams({ calendar_date: "18/11/2026" }, cfg.root, ZONE)).toThrow("calendar_date");
  expect(() => stateFromParams({ calendar_scale: "day" }, config(standardFields(), { scales: ["week"] }).root, ZONE)).toThrow("does not offer the day scale");
  const url = routeUrl({ action: "28", view_type: "calendar", calendar_scale: "year", calendar_date: "2024-02-29" });
  expect(parseRoute(new URL(url, "https://erp.example.test"))).toMatchObject({ view_type: "calendar", calendar_scale: "year", calendar_date: "2024-02-29" });
  for (const bad of [{ calendar_scale: "fortnight" }, { calendar_date: "2025-02-29" }, { calendar_date: "2026-13-01" }]) {
    expect(() => routeUrl({ action: "28", view_type: "calendar", ...bad } as never)).toThrow(ClientError);
  }
});

// ---- the DOM ----

async function mounted(options: Parameters<typeof made>[0] = {}, view: { remove?: boolean; mutations?: Partial<Pick<Parameters<typeof mountCalendar>[0], "access" | "update" | "create">> } = {}) {
  const { model } = made(options);
  await model.load();
  const host = document.createElement("div");
  document.body.append(host);
  const popovers = new PopoverService(document.body); disposers.push(() => popovers.dispose());
  const open = vi.fn(async () => {}), changed = vi.fn(), remove = vi.fn(async () => true), errors: unknown[] = [];
  const pending: Promise<void>[] = [];
  const calendar = mountCalendar({ model, locale: { lang: "en_US", tz: ZONE, date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, popovers,
    run: job => { pending.push(job().catch(error => { errors.push(error); })); }, open, changed, ...view.mutations, ...(view.remove === false ? {} : { remove }) });
  disposers.push(() => calendar.dispose());
  host.append(calendar.element);
  const settle = async () => { while (pending.length) await pending.shift(); };
  return { model, calendar, element: calendar.element, open, changed, remove, errors, settle };
}

/// (partial: a click on an event opens its popover with the shown fields and Edit; Edit opens the record; Delete asks the host and reads the range again; a double click opening the edit surface is not built)
test("the week grid places events and a click opens the popover with the fields, Edit and Delete", async () => {
  const { element, open, remove, settle, model } = await mounted();
  expect(element.querySelector(".calendar-range-label")!.textContent).toMatch(/^Nov 16\s*–\s*22, 2026$/u);
  expect(element.querySelector(".calendar-week-number")!.textContent).toBe("Week 47");
  expect(element.querySelectorAll(".calendar-day-head")).toHaveLength(7);
  const timed = [...element.querySelectorAll<HTMLElement>(".calendar-event.is-timed")];
  expect(timed.map(node => node.querySelector(".calendar-event-title")!.textContent)).toEqual(["Stand-up", "Review", "Lunch"]);
  expect(timed[0]!.style.width).toBe("50%");
  expect(timed[0]!.dataset["color"]).toBe("1");
  const allDay = element.querySelector<HTMLElement>(".calendar-all-day-lane .calendar-event")!;
  expect([allDay.textContent, allDay.style.gridColumn]).toEqual(["Offsite", "4 / 6"]);
  timed[0]!.click();
  const popover = document.querySelector(".calendar-popover")!;
  expect(popover.querySelector("h3")!.textContent).toBe("Stand-up");
  expect([...popover.querySelectorAll("dt")].map(node => node.textContent)).toEqual(["When", "Name", "Owner", "State"]);
  expect([...popover.querySelectorAll("dd")].slice(1).map(node => node.textContent)).toEqual(["Stand-up", "Ana", "Draft"]);
  (popover.querySelector(".erp-button-primary") as HTMLButtonElement).click();
  await settle();
  expect(open).toHaveBeenCalledWith(expect.objectContaining({ id: 1 }));
  expect(document.querySelector(".calendar-popover")).toBeNull();
  const loads = vi.spyOn(model, "load");
  element.querySelector<HTMLElement>(".calendar-event.is-timed")!.click();
  (document.querySelector(".calendar-delete") as HTMLButtonElement).click();
  await settle();
  expect(remove).toHaveBeenCalledWith(expect.objectContaining({ id: 1 }));
  expect(loads).toHaveBeenCalledTimes(1);
});

test("without delete in the arch, or without a host that deletes, the popover has no Delete", async () => {
  const { element } = await mounted({ extra: { delete: false } });
  element.querySelector<HTMLElement>(".calendar-event")!.click();
  expect(document.querySelector(".calendar-delete")).toBeNull();
  expect(document.querySelector(".calendar-popover .erp-button-primary")!.textContent).toBe("Edit");
});

/// (partial: the month grid draws the six weeks with their week numbers; a cell shows at most event_limit events and a "more" button for the rest)
test("the month grid limits each cell and lists the rest", async () => {
  const { element, settle, model, changed } = await mounted({ scale: "month" });
  expect(element.querySelectorAll(".calendar-month-row:not(.calendar-month-heads)")).toHaveLength(6);
  expect(element.querySelector(".calendar-week-cell[role=rowheader]")!.getAttribute("aria-label")).toBe("Week 44");
  const cell = element.querySelector<HTMLElement>('[data-day="2026-11-18"]')!.closest(".calendar-month-cell")!;
  expect(cell.querySelectorAll(".calendar-event")).toHaveLength(2);
  const more = cell.querySelector<HTMLButtonElement>(".calendar-more")!;
  expect(more.textContent).toBe("+1 more");
  more.click();
  expect([...document.querySelectorAll(".calendar-more-list .calendar-event-title")].map(node => node.textContent)).toEqual(["Stand-up", "Review", "Lunch"]);
  (document.querySelector(".calendar-more-list .calendar-event") as HTMLButtonElement).click();
  expect(document.querySelector(".calendar-popover h3")!.textContent).toBe("Stand-up");
  // A day number opens that day in the day scale, and the address follows.
  element.querySelector<HTMLButtonElement>('[data-day="2026-11-19"].calendar-month-day')!.click();
  await settle();
  expect([model.state.scale, model.state.date.toISODate()]).toEqual(["day", "2026-11-19"]);
  expect(changed).toHaveBeenCalledTimes(1);
});

test("the arrow keys move the focused day of the month grid; previous, next and Today navigate", async () => {
  vi.useFakeTimers({ toFake: ["Date"] }); vi.setSystemTime(new Date("2026-11-18T03:00:00Z"));
  const { element, settle, model } = await mounted({ scale: "month", date: "2026-11-18" });
  const grid = element.querySelector<HTMLElement>(".calendar-month-grid")!;
  const focused = grid.querySelector<HTMLButtonElement>('button[data-day="2026-11-18"]')!;
  expect(focused.tabIndex).toBe(0);
  focused.focus();
  focused.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect((document.activeElement as HTMLElement).dataset["day"]).toBe("2026-11-25");
  (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
  expect((document.activeElement as HTMLElement).dataset["day"]).toBe("2026-11-24");
  element.querySelector<HTMLButtonElement>('[aria-label="Next month"]:not([data-mini])')!.click();
  await settle();
  expect(model.state.date.toISODate()).toBe("2026-12-18");
  [...element.querySelectorAll<HTMLButtonElement>(".calendar-toolbar button")].find(node => node.textContent === "Today")!.click();
  await settle();
  expect(model.state.date.toISODate()).toBe("2026-11-18");
  [...element.querySelectorAll<HTMLButtonElement>(".calendar-scale")].find(node => node.textContent === "Year")!.click();
  await settle();
  expect(element.querySelectorAll(".calendar-year-month")).toHaveLength(12);
  expect(element.querySelector('[data-day="2026-11-18"].has-events')!.getAttribute("aria-label")).toBe("Wednesday, November 18, 2026, 3 events");
});

test("today is marked in the month cell, the week heads, the all-day row and the time grid", async () => {
  vi.useFakeTimers({ toFake: ["Date"] }); vi.setSystemTime(new Date("2026-11-18T03:00:00Z"));
  const week = await mounted({ scale: "week", date: "2026-11-18" });
  expect([...week.element.querySelectorAll(".calendar-day-head")].map(node => node.classList.contains("is-today"))).toEqual([false, false, true, false, false, false, false]);
  expect(week.element.querySelectorAll(".calendar-day-column.is-today")).toHaveLength(1);
  const mark = week.element.querySelector<HTMLElement>(".calendar-all-day-lane > .calendar-all-day-today")!;
  expect(mark.style.getPropertyValue("--calendar-today")).toBe("2"); // Wednesday in a Monday-first week
  expect(mark.getAttribute("aria-hidden")).toBe("true");
  const month = await mounted({ scale: "month", date: "2026-11-18" });
  expect([...month.element.querySelectorAll<HTMLElement>(".calendar-month-cell.is-today")].map(cell => cell.querySelector<HTMLElement>("[data-day]")!.dataset["day"])).toEqual(["2026-11-18"]);
  const later = await mounted({ scale: "week", date: "2026-12-02" });
  expect(later.element.querySelector(".calendar-all-day-today")).toBeNull();
});

test("the side panel's mini calendar navigates and its filters hide events without a read", async () => {
  const { element, settle, model } = await mounted();
  const loads = vi.spyOn(model, "load");
  const mini = element.querySelector<HTMLElement>(".calendar-mini")!;
  expect(mini.querySelectorAll(".calendar-mini-day.in-range")).toHaveLength(7);
  mini.querySelector<HTMLButtonElement>('[data-mini="next"]')!.click();
  expect(element.querySelector(".calendar-mini-label")!.textContent).toBe("December 2026");
  element.querySelector<HTMLButtonElement>('.calendar-mini [data-day="2026-12-02"]')!.click();
  await settle();
  expect(model.state.date.toISODate()).toBe("2026-12-02");
  expect(loads).toHaveBeenCalledTimes(1);
  expect(element.querySelector(".calendar-mini-day[aria-current=date]")!.getAttribute("data-day")).toBe("2026-12-02");
});

test("filters: a value checkbox hides its events, All brings them back, a section folds", async () => {
  const { element, model } = await mounted();
  const owners = () => element.querySelectorAll<HTMLElement>(".calendar-filter-section")[0]!;
  const box = (label: string) => [...owners().querySelectorAll("label")].find(node => node.textContent === label)!.querySelector("input")!;
  expect(owners().querySelector(".calendar-swatch[data-color='1']")).not.toBeNull();
  box("Ana").click();
  expect(element.querySelectorAll(".calendar-event")).toHaveLength(2);
  expect(box("All").indeterminate).toBe(true);
  expect(document.activeElement).toBe(box("Ana"));
  box("All").click();
  expect(element.querySelectorAll(".calendar-event")).toHaveLength(4);
  expect(model.visible()).toHaveLength(4);
  const head = owners().querySelector<HTMLButtonElement>(".calendar-filter-head")!;
  head.click();
  expect(owners().querySelector<HTMLElement>(".calendar-filter-values")!.hidden).toBe(true);
  expect(owners().querySelector(".calendar-filter-head")!.getAttribute("aria-expanded")).toBe("false");
});

/// (partial: a calendar opened on a range past the bound shows the refusal naming the setting in its error state; cycle 17 review, TypeScript MUST 1)
test("a first read past the bound opens the calendar with the refusal that names the setting", async () => {
  const { model } = made({ max: 3 });
  const refused = await model.load().then(() => undefined, (error: unknown) => error as Error);
  expect(refused?.message).toMatch(/web\.calendar_max_events/);
  const popovers = new PopoverService(document.body); disposers.push(() => popovers.dispose());
  const calendar = mountCalendar({ model, locale: { lang: "en_US", tz: ZONE, date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, popovers,
    run: job => { void job().catch(() => undefined); }, open: vi.fn(async () => {}), changed: vi.fn(), failed: refused!.message });
  disposers.push(() => calendar.dispose());
  document.body.append(calendar.element);
  const status = calendar.element.querySelector(".calendar-status.is-error")!;
  expect(status.getAttribute("role")).toBe("alert");
  expect(status.textContent).toBe(refused!.message);
});

test("an empty range says so, a refused range shows the error state, and the side panel folds behind its toggle", async () => {
  const empty = await mounted({ source: [] });
  expect(empty.element.querySelector(".calendar-status")!.textContent).toBe("No events in this range.");
  const toggle = empty.element.querySelector<HTMLButtonElement>(".calendar-side-toggle")!;
  expect(toggle.getAttribute("aria-expanded")).toBe("true");
  toggle.click();
  expect(empty.element.querySelector<HTMLElement>(".calendar-side")!.hidden).toBe(true);
  expect(empty.element.querySelector(".calendar-side-toggle")!.getAttribute("aria-expanded")).toBe("false");
  const crowded = await mounted({ max: 3, source: records.slice(0, 3) });
  crowded.model.limits.maxEvents = 2; // the bound is now passed: the next navigation is refused
  crowded.element.querySelector<HTMLButtonElement>('[aria-label="Next week"]')!.click();
  await crowded.settle();
  expect(String(crowded.errors[0])).toMatch(/web\.calendar_max_events/);
  expect(crowded.element.querySelector(".calendar-status.is-error")!.getAttribute("role")).toBe("alert");
  expect(crowded.element.querySelector(".calendar-time-grid")).toBeNull();
});

test("the calendar passes the component accessibility audit in each scale", async () => {
  for (const scale of ["week", "month", "year"] as const) {
    const { element } = await mounted({ scale });
    await auditComponent(element);
    document.body.replaceChildren();
  }
});

test("an event moves across a month in local time, resizes independently and keeps its dates on refusal", async () => {
  const source = [{ ...records[0], start: "2026-11-29 17:30:00", stop: "2026-11-29 19:00:00" }];
  const update = vi.fn(async (_event: unknown, vals: Record<string, string>) => { Object.assign(source[0]!, vals); });
  const { element, settle, errors } = await mounted({ source, scale: "month", date: "2026-11-30" }, {
    mutations: { update, access: async () => ({ edit: true, remove: false }) }
  });
  const editor = async (): Promise<HTMLFormElement> => {
    element.querySelector<HTMLButtonElement>(".calendar-event")!.click(); await settle();
    [...document.querySelectorAll("button")].find(btn => btn.textContent === "Change dates")!.click();
    return document.querySelector(".calendar-editor")!;
  };
  const form = await editor(), [start, end] = form.querySelectorAll("input");
  expect(start!.value).toMatch(/^2026-11-30T00:30/);
  start!.value = "2026-12-01T00:30"; start!.dispatchEvent(new Event("change"));
  expect(end!.value).toMatch(/^2026-12-01T02:00/);
  form.requestSubmit(); await settle();
  expect(update.mock.calls[0]![1]).toEqual({ start: "2026-11-30 17:30:00", stop: "2026-11-30 19:00:00" });
  const resize = await editor(); resize.querySelectorAll("input")[1]!.value = "2026-12-01T03:00";
  resize.requestSubmit(); await settle();
  expect(update.mock.calls[1]![1]).toEqual({ start: "2026-11-30 17:30:00", stop: "2026-11-30 20:00:00" });
  update.mockRejectedValueOnce(new Error("write refused"));
  const denied = await editor(); denied.querySelectorAll("input")[0]!.value = "2026-11-30T00:30";
  denied.requestSubmit(); await settle();
  expect(errors).toEqual([expect.objectContaining({ message: "write refused" })]);
  expect(source[0]!.start).toBe("2026-11-30 17:30:00");
});

test("read-only record opens without mutation controls and quick creation uses local midnight across a UTC date boundary", async () => {
  const create = vi.fn(async () => {}), update = vi.fn(async () => {});
  const { element, settle, open } = await mounted({}, { mutations: { create, update, access: async () => ({ edit: false, remove: false }) } });
  element.querySelector<HTMLButtonElement>(".calendar-event")!.click(); await settle();
  expect(document.querySelector(".calendar-popover-actions")!.textContent).toBe("Open");
  document.querySelector<HTMLButtonElement>(".calendar-popover-actions button")!.click(); await settle();
  expect(open).toHaveBeenCalledTimes(1);
  [...element.querySelectorAll("button")].find(btn => btn.textContent === "New event")!.click();
  const form = document.querySelector<HTMLFormElement>(".calendar-editor")!;
  const [name, start, end] = form.querySelectorAll("input");
  name!.value = "Early visit"; start!.value = "2026-12-01T00:30"; end!.value = "2026-12-01T01:00";
  form.requestSubmit(); await settle();
  expect(create).toHaveBeenCalledWith({ start: "2026-11-30 17:30:00", stop: "2026-11-30 18:00:00" }, "Early visit");
  expect(update).not.toHaveBeenCalled();
});

test("multi-day bars split at the week boundary and cross into the adjacent month", async () => {
  const { element } = await mounted({ scale: "month", source: [{ ...records[2], start: "2026-11-27 17:00:00", stop: "2026-12-02 05:00:00" }] });
  const bars = [...element.querySelectorAll<HTMLElement>(".calendar-month-event")];
  expect(bars.map(bar => bar.style.getPropertyValue("--span-days"))).toEqual(["2", "3"]);
  expect(bars[0]!.classList.contains("continues-after")).toBe(true);
  expect(bars[1]!.classList.contains("continues-before")).toBe(true);
});

test("the hourly grid uses local clock labels on spring-forward and fall-back days", () => {
  const zone = "America/New_York", cfg = config();
  for (const [date, from, to] of [
    ["2026-03-08","2026-03-08 14:15:00","2026-03-08 14:45:00"],
    ["2026-11-01","2026-11-01 15:15:00","2026-11-01 15:45:00"],
  ] as const) {
    const event = decodeEvent(cfg.root, { ...records[0], start: from, stop: to }, zone)!;
    const placed = layoutDay([event], DateTime.fromISO(date, {zone}));
    expect([placed[0]!.top, placed[0]!.height]).toEqual([615,30]);
  }
});

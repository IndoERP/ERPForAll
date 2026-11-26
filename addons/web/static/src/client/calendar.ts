/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The calendar view's model: the visible range of a scale and a date in the user's zone, the
// range read under the search domain (bounded by `web.calendar_max_events`), events placed by start and stop, colours, side-panel
// filters pruned in memory, and the time-grid layout. The DOM is `calendar-view.ts`.
import { DateTime } from "luxon";
import { array, integer, object, text, type CalendarFieldNode, type CalendarNode, type CalendarScale, type Field, type View } from "./metadata.js";
import { ClientError } from "./transport.js";

export const CALENDAR_SCALES: readonly CalendarScale[] = ["day", "week", "month", "year"];
/** Number of palette colours (`--palette-1` to `--palette-11`). */
export const PALETTE_SIZE = 11;

export interface CalendarConfig {
  root: CalendarNode;
  /** The `<field>` children, in arch order. */
  fields: CalendarFieldNode[];
  /** Every field the read asks for (the view's field set). */
  readFields: readonly Field[];
}
export function readCalendarView(view: View): CalendarConfig {
  const root = view.nodes[0];
  if (view.type !== "calendar" || !root || root.kind !== "calendar") throw new ClientError("server", "The calendar description has no calendar root.");
  const fields = root.children.map(child => {
    if (child.kind !== "calendar_field") throw new ClientError("server", "A calendar holds only calendar fields.");
    return child;
  });
  return { root, fields, readFields: view.fields };
}

/** Visible range: `start` inclusive, `end` exclusive, both at midnight in the user's zone. */
export interface CalendarRange { start: DateTime; end: DateTime }
/** Weeks start on Monday, as the date picker does (`datetime-editor.ts`): the wire carries no week-start setting yet. */
export function computeRange(scale: CalendarScale, date: DateTime, monthOverflow: boolean): CalendarRange {
  const day = date.startOf("day");
  if (scale === "day") return { start: day, end: day.plus({ days: 1 }) };
  if (scale === "week") { const start = day.minus({ days: day.weekday - 1 }); return { start, end: start.plus({ weeks: 1 }) }; }
  if (scale === "year") { const start = day.startOf("year"); return { start, end: start.plus({ years: 1 }) }; }
  const first = day.startOf("month");
  if (!monthOverflow) return { start: first, end: first.plus({ months: 1 }) };
  const start = first.minus({ days: first.weekday - 1 });
  return { start, end: start.plus({ weeks: 6 }) };
}
/** One scale step forward or back from `date`. */
export function step(scale: CalendarScale, date: DateTime, direction: 1 | -1): DateTime {
  return date.plus({ [scale === "day" ? "days" : scale === "week" ? "weeks" : scale === "month" ? "months" : "years"]: direction });
}

const utcWire = (value: DateTime): string => value.toUTC().toFormat("yyyy-MM-dd HH:mm:ss");
const dateWire = (value: DateTime): string => value.toFormat("yyyy-MM-dd");
/** Events that start before the range ends and stop after it starts; an event without a stop value counts from its start (reference `calendar_model.js` computeRangeDomain). */
export function rangeDomain(root: CalendarNode, range: CalendarRange): unknown[] {
  const isDate = root.dateStart.kind === "date";
  // Dates compare as days (the last visible day is inclusive); datetimes as UTC instants up to the last second of the range.
  const first = isDate ? dateWire(range.start) : utcWire(range.start);
  const last = isDate ? dateWire(range.end.minus({ days: 1 })) : utcWire(range.end.minus({ seconds: 1 }));
  const start = root.dateStart.name, stop = root.dateStop?.name ?? start;
  if (stop === start) return [[start, "<=", last], [start, ">=", first]];
  return ["&", [start, "<=", last], "|", [stop, ">=", first], "&", [stop, "=", false], [start, ">=", first]];
}

export interface CalendarEvent {
  id: number;
  title: string;
  /** In the user's zone; `end` exclusive. An all-day event covers whole days from `start` to `end`. */
  start: DateTime;
  end: DateTime;
  allDay: boolean;
  /** 1 to 11, or 0 for no colour. */
  color: number;
  record: Record<string, unknown>;
}
const DATE_WIRE = /^\d{4}-\d{2}-\d{2}$/, DATETIME_WIRE = /^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/;
function wireValue(field: Field, raw: unknown, zone: string): DateTime | null {
  if (raw === false || raw === null || raw === undefined) return null;
  const value = text(raw, `${field.name} value`);
  const parsed = field.kind === "date"
    ? DATE_WIRE.test(value) ? DateTime.fromISO(value, { zone }) : undefined
    : DATETIME_WIRE.test(value) ? DateTime.fromSQL(value, { zone: "UTC" }).setZone(zone) : undefined;
  if (!parsed?.isValid) throw new ClientError("server", `Invalid ${field.kind} value for ${field.name}.`);
  return parsed;
}
/** The palette index of a colour value: a many2one id or an integer, cycled over the palette; no value is no colour. */
export function colorIndex(value: unknown): number {
  const n = typeof value === "number" ? value : value && typeof value === "object" ? (value as Record<string, unknown>)["id"] : null;
  if (typeof n !== "number" || !Number.isSafeInteger(n) || n <= 0) return 0;
  return ((n - 1) % PALETTE_SIZE) + 1;
}
/** A timed event without a stop, or one that stops where it starts, is drawn this long (as the reference draws a short line). */
const MIN_MINUTES = 30;
export function decodeEvent(root: CalendarNode, raw: unknown, zone: string): CalendarEvent | null {
  const record = object(raw, "calendar record");
  const id = integer(record["id"], "calendar record id");
  const start = wireValue(root.dateStart, record[root.dateStart.name], zone);
  if (!start) return null; // the range domain never returns one; an unscheduled record is not drawn
  const stop = root.dateStop ? wireValue(root.dateStop, record[root.dateStop.name], zone) : null;
  const allDay = root.dateStart.kind === "date" || Boolean(root.allDay && record[root.allDay.name] === true);
  let end: DateTime;
  if (allDay) end = (stop && stop > start ? stop : start).startOf("day").plus({ days: 1 });
  else end = stop && stop > start ? stop : start.plus({ minutes: MIN_MINUTES });
  const title = typeof record["display_name"] === "string" && record["display_name"] ? record["display_name"] : `#${id}`;
  return { id, title, start: allDay ? start.startOf("day") : start, end, allDay, color: root.color ? colorIndex(record[root.color.name]) : 0, record };
}

// ---- side-panel filters ----

export interface FilterValue { key: string; label: string; color: number; active: boolean }
export interface FilterSection { field: Field; label: string; values: FilterValue[]; collapsed: boolean }
function filterKey(field: Field, value: unknown): { key: string; label: string } {
  if (value === false || value === null || value === undefined) return { key: "false", label: "Not set" };
  if (field.kind === "many2one") {
    const item = object(value, `${field.name} value`);
    return { key: String(integer(item["id"], `${field.name} id`)), label: text(item["display_name"], `${field.name} name`) };
  }
  const key = text(value, `${field.name} value`);
  return { key, label: field.selection?.find(([choice]) => choice === key)?.[1] ?? key };
}

/** The bounds the calendar reads under, from Settings. */
export interface CalendarLimits { maxEvents: number }
export type CalendarRead = (request: { domain: unknown[]; limit: number }) => Promise<unknown>;
export interface CalendarState { scale: CalendarScale; date: DateTime }

export class CalendarModel {
  state: CalendarState;
  events: CalendarEvent[] = [];
  sections: FilterSection[];
  /** Inactive filter keys by field name: kept across reads, so a value turned off stays off. */
  private readonly inactive = new Map<string, Set<string>>();
  private request = 0;
  constructor(readonly config: CalendarConfig, private readonly read: CalendarRead, private readonly domain: () => unknown[],
    readonly limits: CalendarLimits, readonly zone: string, initial: CalendarState) {
    if (!config.root.scales.includes(initial.scale)) throw new ClientError("validation", `This calendar does not offer the ${initial.scale} scale.`);
    this.state = initial;
    this.sections = config.fields.filter(item => item.filters).map(item => ({ field: item.field, label: item.label, values: [], collapsed: false }));
  }
  get range(): CalendarRange { return computeRange(this.state.scale, this.state.date, this.config.root.monthOverflow); }
  today(): DateTime { return DateTime.now().setZone(this.zone).startOf("day"); }
  /** Reads the events of the visible range; a read that a newer one overtook changes nothing. */
  async load(): Promise<void> {
    const mine = ++this.request, { maxEvents } = this.limits;
    const answer = object(await this.read({ domain: [...this.domain(), ...rangeDomain(this.config.root, this.range)], limit: maxEvents + 1 }), "calendar read");
    if (mine !== this.request) return;
    const records = array(answer["records"], "calendar records");
    if (records.length > maxEvents) {
      this.events = [];
      throw new ClientError("validation", `This range has more than ${maxEvents} events, the most a calendar shows (setting web.calendar_max_events). Choose a shorter scale or narrow the search.`);
    }
    this.events = records.map(raw => decodeEvent(this.config.root, raw, this.zone)).filter((event): event is CalendarEvent => event !== null);
    this.rebuildSections();
  }
  async go(next: Partial<CalendarState>): Promise<void> {
    const scale = next.scale ?? this.state.scale;
    if (!this.config.root.scales.includes(scale)) throw new ClientError("validation", `This calendar does not offer the ${scale} scale.`);
    this.state = { scale, date: (next.date ?? this.state.date).startOf("day") };
    await this.load();
  }
  private rebuildSections(): void {
    for (const section of this.sections) {
      const seen = new Map<string, FilterValue>(), off = this.inactive.get(section.field.name);
      for (const event of this.events) {
        const raw = event.record[section.field.name], { key, label } = filterKey(section.field, raw);
        if (!seen.has(key)) seen.set(key, { key, label, color: this.config.root.color?.name === section.field.name ? colorIndex(raw) : 0, active: !off?.has(key) });
      }
      section.values = [...seen.values()].sort((a, b) => (a.key === "false" ? 1 : 0) - (b.key === "false" ? 1 : 0) || a.label.localeCompare(b.label));
    }
  }
  toggle(fieldName: string, key: string): void {
    const off = this.inactive.get(fieldName) ?? new Set<string>();
    if (off.has(key)) off.delete(key); else off.add(key);
    this.inactive.set(fieldName, off);
    this.rebuildSections();
  }
  /** Turns every value of a section on, or off when all are on. */
  toggleAll(fieldName: string): void {
    const section = this.sections.find(item => item.field.name === fieldName);
    if (!section) throw new ClientError("validation", `No filter section for ${fieldName}.`);
    const allOn = section.values.every(value => value.active);
    this.inactive.set(fieldName, new Set(allOn ? section.values.map(value => value.key) : []));
    this.rebuildSections();
  }
  /** The events the side-panel filters let through (reference: pruned in memory, `calendar_model.js` dynamic filters). */
  visible(): CalendarEvent[] {
    return this.events.filter(event => this.sections.every(section => !this.inactive.get(section.field.name)?.has(filterKey(section.field, event.record[section.field.name]).key)));
  }
}

// ---- layout ----

/** Events touching one day, all-day ones first, then by start. */
export function eventsOn(events: readonly CalendarEvent[], day: DateTime): CalendarEvent[] {
  const next = day.plus({ days: 1 });
  return events.filter(event => event.start < next && event.end > day)
    .sort((a, b) => Number(b.allDay) - Number(a.allDay) || a.start.toMillis() - b.start.toMillis() || a.id - b.id);
}
export interface TimedPlacement { event: CalendarEvent; top: number; height: number; column: number; columns: number }
/** Timed events of one day as minutes from midnight, overlapping ones side by side: each cluster of overlapping events shares its columns. */
export function layoutDay(events: readonly CalendarEvent[], day: DateTime): TimedPlacement[] {
  const next = day.plus({ days: 1 });
  const clockMinute = (instant: DateTime): number => {
    const local = instant.setZone(day.zone);
    return local.hour * 60 + local.minute + local.second / 60;
  };
  const startMinute = (event: CalendarEvent): number => event.start <= day ? 0 : clockMinute(event.start);
  const timed = eventsOn(events, day).filter(event => !event.allDay).sort((a, b) => startMinute(a) - startMinute(b));
  const placed: TimedPlacement[] = [];
  let cluster: TimedPlacement[] = [], clusterEnd = -1, ends: number[] = [];
  const close = (): void => { for (const item of cluster) item.columns = ends.length; cluster = []; ends = []; };
  for (const event of timed) {
    const top = startMinute(event);
    const bottom = event.end >= next ? 24 * 60 : clockMinute(event.end);
    const height = Math.max(MIN_MINUTES / 2, bottom - top);
    if (top >= clusterEnd) close();
    let column = ends.findIndex(end => end <= top);
    if (column < 0) { column = ends.length; ends.push(top + height); } else ends[column] = top + height;
    const item = { event, top, height, column, columns: 1 };
    cluster.push(item); placed.push(item);
    clusterEnd = Math.max(clusterEnd, top + height);
  }
  close();
  return placed;
}

// ---- address ----

export const CALENDAR_PARAMS = ["calendar_scale", "calendar_date"] as const;
export interface CalendarParams { calendar_scale?: CalendarScale; calendar_date?: string }
export function stateFromParams(params: CalendarParams, root: CalendarNode, zone: string): CalendarState {
  const scale = params.calendar_scale ?? root.mode;
  if (!root.scales.includes(scale)) throw new ClientError("validation", `This calendar does not offer the ${scale} scale.`);
  const date = params.calendar_date === undefined ? DateTime.now().setZone(zone) : DateTime.fromISO(params.calendar_date, { zone });
  if (!date.isValid || params.calendar_date !== undefined && !DATE_WIRE.test(params.calendar_date)) throw new ClientError("validation", "Invalid navigation parameter: calendar_date.");
  return { scale, date: date.startOf("day") };
}
export function paramsFromState(state: CalendarState): Required<CalendarParams> {
  return { calendar_scale: state.scale, calendar_date: dateWire(state.date) };
}

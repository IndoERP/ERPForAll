/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/*! Luxon 3.7.2 — upstream MIT licence notice.
Copyright 2019 JS Foundation and other contributors

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
*/
import { DateTime, IANAZone } from "luxon";
import type { Locale } from "../generated/human-ui.js";
import { ClientError } from "./transport.js";

export type EditorLocale = Pick<Locale, "lang" | "tz" | "date_format" | "time_format">;
export type EditorValue = string | false;
export type DateKind = "date" | "datetime";
export interface DateParts { year: number; month: number; day: number; hour: number; minute: number; second: number }
export type ParseResult = { ok: true; value: EditorValue } | { ok: false; error: ClientError };
export interface DateParseOptions { previous?: EditorValue; showSeconds?: boolean }
export interface TimeParseOptions extends DateParseOptions { rounding?: number }

/** Admission bounds, not performance measurements; parsing and suggestion work is finite. */
export const DATETIME_INPUT_LIMIT = 256;
export const DATETIME_FORMAT_LIMIT = 128;
const DATE_WIRE = /^(\d{4})-(\d{2})-(\d{2})$/;
const DATETIME_WIRE = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})$/;
const TIME_WIRE = /^(\d{2}):(\d{2}):(\d{2})$/;
const ISO_INPUT = /^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2})(?::(\d{2}))?(Z|[+-]\d{2}:\d{2})?$/i;
const NATIVE_TIME = /^(\d{1,2}):(\d{2})(?::(\d{2}))?$/;
const DIRECTIVES = {
  Y: "yyyy", y: "yy", m: "MM", d: "dd", H: "HH", I: "hh", M: "mm", S: "ss",
  p: "a", a: "ccc", A: "cccc", b: "LLL", B: "LLLL",
} as const;
type Directive = keyof typeof DIRECTIVES;
type Segment = { kind: "literal"; value: string } | { kind: "directive"; code: Directive };

function fail(message: string): ParseResult { return { ok: false, error: new ClientError("validation", message) }; }
const messageFor = (kind: DateKind): string => kind === "date" ? "Enter a valid date." : "Enter a valid date and time.";
const language = (locale: EditorLocale): string => locale.lang.replaceAll("_", "-");
function dateKind(kind: DateKind): void {
  if (kind !== "date" && kind !== "datetime") throw new ClientError("validation", "Invalid date/time editor kind.");
}
function bool(value: unknown): void {
  if (value !== undefined && typeof value !== "boolean") throw new ClientError("validation", "Invalid seconds visibility option.");
}
function checkedOptions(options: unknown, allowed: readonly string[]): void {
  if (options === null || typeof options !== "object" || Array.isArray(options) ||
    Reflect.ownKeys(options).some(key => typeof key !== "string" || !allowed.includes(key))) {
    throw new ClientError("validation", "Invalid date/time conversion options.");
  }
}
function boundedInput(input: string): ParseResult | undefined {
  if (typeof input !== "string") return fail("Date/time input must be text.");
  if (input.length > DATETIME_INPUT_LIMIT) return fail(`Date/time input exceeds ${DATETIME_INPUT_LIMIT} characters.`);
  return undefined;
}
function formatSegments(format: string): Segment[] {
  if (format.length > DATETIME_FORMAT_LIMIT) throw new ClientError("validation", `Date/time format exceeds ${DATETIME_FORMAT_LIMIT} characters.`);
  const segments: Segment[] = [];
  const literal = (value: string): void => {
    const last = segments.at(-1);
    if (last?.kind === "literal") last.value += value;
    else segments.push({ kind: "literal", value });
  };
  for (let index = 0; index < format.length; index++) {
    const character = format[index]!;
    if (character !== "%") { literal(character); continue; }
    const code = format[++index];
    if (code === undefined) throw new ClientError("unsupported", "Incomplete date/time format directive.");
    if (code === "%") literal("%");
    else if (Object.hasOwn(DIRECTIVES, code)) segments.push({ kind: "directive", code: code as Directive });
    else throw new ClientError("unsupported", `Unsupported date/time format directive: %${code}.`);
  }
  return segments;
}
function validateFormat(segments: readonly Segment[], kind: "date" | "time"): void {
  const fields: Record<Directive, string> = { Y: "year", y: "year", m: "month", b: "month", B: "month", d: "day", H: "hour", I: "hour", M: "minute", S: "second", p: "meridiem", a: "weekday", A: "weekday" };
  const seen = new Set<string>();
  for (const segment of segments) {
    if (segment.kind !== "directive") continue;
    const field = fields[segment.code];
    if (seen.has(field)) throw new ClientError("unsupported", `Repeated date/time format component: ${field}.`);
    seen.add(field);
  }
  const codes = new Set(segments.flatMap(segment => segment.kind === "directive" ? [segment.code] : []));
  const any = (values: readonly Directive[]): boolean => values.some(value => codes.has(value));
  if (kind === "date") {
    if (!any(["Y", "y"]) || !any(["m", "b", "B"]) || !codes.has("d") || any(["H", "I", "M", "S", "p"])) {
      throw new ClientError("unsupported", "Date format must contain a year, month and day without time components.");
    }
  } else if (!any(["H", "I"]) || !codes.has("M") || any(["Y", "y", "m", "d", "a", "A", "b", "B"]) ||
    codes.has("H") && codes.has("I") || codes.has("I") !== codes.has("p") || codes.has("H") && codes.has("p")) {
    throw new ClientError("unsupported", "Time format must contain hours and minutes, with AM/PM only for a 12-hour clock.");
  }
}

/** Decode the bootstrap contract without requiring any editor capability at application startup. */
export function decodeEditorLocale(raw: unknown): EditorLocale {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new ClientError("server", "Invalid editor locale.");
  const source = raw as Record<string, unknown>;
  const member = (key: keyof EditorLocale): string => {
    const value = source[key];
    if (typeof value !== "string" || !value.trim()) throw new ClientError("server", `Invalid editor locale ${key}.`);
    return value;
  };
  const locale: EditorLocale = { lang: member("lang"), tz: member("tz"), date_format: member("date_format"), time_format: member("time_format") };
  if (locale.lang.length > 128 || locale.tz.length > 128) throw new ClientError("validation", "Editor language and timezone must not exceed 128 characters.");
  return Object.freeze(locale);
}

/** Internal capability admission checks only the formats consumed by this control. */
export function editorLocaleFor(kind: DateKind | "time", raw: unknown): EditorLocale {
  if (kind !== "date" && kind !== "datetime" && kind !== "time") throw new ClientError("validation", "Invalid date/time editor kind.");
  const locale = decodeEditorLocale(raw);
  let supported: string[];
  try { supported = Intl.DateTimeFormat.supportedLocalesOf([language(locale)]); }
  catch { throw new ClientError("unsupported", `Unsupported editor language: ${locale.lang}.`); }
  if (!supported.length) throw new ClientError("unsupported", `Unsupported editor language: ${locale.lang}.`);
  if (kind !== "time" && !IANAZone.isValidZone(locale.tz)) throw new ClientError("unsupported", `Unsupported editor timezone: ${locale.tz}.`);
  if (kind !== "time") validateFormat(formatSegments(locale.date_format), "date");
  if (kind !== "date") validateFormat(formatSegments(locale.time_format), "time");
  return locale;
}

/** Strict public admission validates both date and time capabilities. */
export function editorLocale(raw: unknown): EditorLocale {
  return editorLocaleFor("datetime", raw);
}

function asParts(value: DateTime): DateParts {
  return { year: value.year, month: value.month, day: value.day, hour: value.hour, minute: value.minute, second: value.second };
}
function validParts(parts: DateParts): boolean {
  return parts !== null && typeof parts === "object" &&
    [parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second].every(value => Number.isInteger(value)) &&
    parts.year >= 1 && parts.year <= 9999 && parts.month >= 1 && parts.month <= 12 && parts.day >= 1 && parts.day <= 31 &&
    parts.hour >= 0 && parts.hour <= 23 && parts.minute >= 0 && parts.minute <= 59 && parts.second >= 0 && parts.second <= 59;
}
function utcDate(parts: DateParts): DateTime | undefined {
  if (!validParts(parts)) return undefined;
  const value = DateTime.fromObject(parts, { zone: "UTC" });
  return value.isValid ? value : undefined;
}
function sameParts(a: DateParts, b: DateParts): boolean {
  return a.year === b.year && a.month === b.month && a.day === b.day && a.hour === b.hour && a.minute === b.minute && a.second === b.second;
}
function canonicalUtc(kind: DateKind, value: EditorValue): DateTime | null {
  if (value === false) return null;
  const match = typeof value === "string" ? value.match(kind === "date" ? DATE_WIRE : DATETIME_WIRE) : null;
  const parts: DateParts = { year: Number(match?.[1]), month: Number(match?.[2]), day: Number(match?.[3]), hour: Number(match?.[4] ?? 0), minute: Number(match?.[5] ?? 0), second: Number(match?.[6] ?? 0) };
  const parsed = match ? utcDate(parts) : undefined;
  if (!parsed) throw new ClientError("server", `Invalid represented ${kind} value.`);
  return parsed;
}
function canonical(kind: DateKind, value: EditorValue, locale: EditorLocale): DateTime | null {
  const parsed = canonicalUtc(kind, value);
  return parsed === null || kind === "date" ? parsed : parsed.setZone(locale.tz);
}
function canonicalTime(value: EditorValue): DateTime | null {
  if (value === false) return null;
  const match = typeof value === "string" ? value.match(TIME_WIRE) : null;
  const parsed = match ? utcDate({ year: 2000, month: 1, day: 1, hour: Number(match[1]), minute: Number(match[2]), second: Number(match[3]) }) : undefined;
  if (!parsed) throw new ClientError("server", "Invalid represented time value.");
  return parsed;
}
/** Validate represented wire values independently of recoverable locale configuration. */
export function assertEditorValue(kind: DateKind | "time", value: EditorValue): void {
  if (kind === "time") canonicalTime(value);
  else { dateKind(kind); canonicalUtc(kind, value); }
}
function withoutSeconds(source: readonly Segment[]): Segment[] {
  const segments = source.map(segment => ({ ...segment }));
  const index = segments.findIndex(segment => segment.kind === "directive" && segment.code === "S");
  if (index < 0) return segments;
  const before = segments[index - 1], after = segments[index + 1];
  if (before?.kind === "literal" && /[:.,\s]$/.test(before.value)) before.value = before.value.slice(0, -1);
  else if (after?.kind === "literal" && /^[:.,\s]/.test(after.value)) after.value = after.value.slice(1);
  segments.splice(index, 1);
  return segments;
}
function timeFormat(locale: EditorLocale, showSeconds: boolean): Segment[] {
  const segments = formatSegments(locale.time_format);
  return showSeconds ? segments : withoutSeconds(segments);
}
function dateTimeFormat(kind: DateKind, locale: EditorLocale, showSeconds: boolean): Segment[] {
  const date = formatSegments(locale.date_format);
  return kind === "date" ? date : [...date, { kind: "literal", value: " " }, ...timeFormat(locale, showSeconds)];
}
function render(value: DateTime, segments: readonly Segment[], locale: EditorLocale): string {
  return segments.map(segment => segment.kind === "literal" ? segment.value : value.toFormat(DIRECTIVES[segment.code], { locale: language(locale), numberingSystem: "latn", outputCalendar: "gregory" })).join("");
}
function wire(value: DateTime, kind: DateKind): string {
  return value.toUTC().toFormat(kind === "date" ? "yyyy-MM-dd" : "yyyy-MM-dd HH:mm:ss", { numberingSystem: "latn", outputCalendar: "gregory" });
}
function localValue(parts: DateParts, locale: EditorLocale, previous?: EditorValue): ParseResult {
  if (!utcDate(parts)) return fail("Enter a valid date and time.");
  const result = DateTime.fromObject(parts, { zone: locale.tz });
  if (!result.isValid) return fail("Enter a valid date and time.");
  if (!sameParts(parts, asParts(result))) return fail(`This local time does not exist in ${locale.tz}.`);
  const possible = result.getPossibleOffsets();
  if (possible.length > 1) {
    const represented = previous === undefined ? null : canonical("datetime", previous, locale);
    if (represented && sameParts(parts, asParts(represented)) && possible.some(candidate => candidate.toMillis() === represented.toMillis())) return { ok: true, value: previous! };
    return fail(`This local time occurs twice in ${locale.tz}. Enter an ISO datetime with an explicit offset.`);
  }
  const utc = result.toUTC();
  if (utc.year < 1 || utc.year > 9999) return fail("Enter a valid date and time.");
  return { ok: true, value: wire(utc, "datetime") };
}

/** Render a canonical field value in the explicitly configured language and timezone. */
export function formatEditorValue(kind: DateKind, value: EditorValue, locale: EditorLocale, options: { showSeconds?: boolean } = {}): string {
  dateKind(kind); checkedOptions(options, ["showSeconds"]); const configured = editorLocaleFor(kind, locale); bool(options.showSeconds);
  const parsed = canonical(kind, value, configured);
  return parsed ? render(parsed, dateTimeFormat(kind, configured, options.showSeconds ?? true), configured) : "";
}
export function partsFromValue(kind: DateKind, value: EditorValue, locale: EditorLocale): DateParts | null {
  dateKind(kind); const parsed = canonical(kind, value, editorLocaleFor(kind, locale));
  return parsed ? asParts(parsed) : null;
}
export function valueFromParts(kind: DateKind, parts: DateParts, locale: EditorLocale, previous?: EditorValue): ParseResult {
  dateKind(kind); const configured = editorLocaleFor(kind, locale);
  if (previous !== undefined) canonical(kind, previous, configured);
  const value = utcDate(parts);
  if (!value) return fail(messageFor(kind));
  return kind === "date" ? { ok: true, value: wire(value, "date") } : localValue(parts, configured, previous);
}

const escaped = (value: string): string => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
function nameValues(code: Directive, locale: EditorLocale): string[] {
  const token = DIRECTIVES[code];
  if (code === "p") return [0, 12].map(hour => DateTime.utc(2000, 1, 1, hour).toFormat(token, { locale: language(locale) }));
  if (code === "a" || code === "A") return Array.from({ length: 7 }, (_, index) => DateTime.utc(2000, 1, 3 + index).toFormat(token, { locale: language(locale) }));
  return Array.from({ length: 12 }, (_, index) => DateTime.utc(2000, index + 1, 1).toFormat(token, { locale: language(locale) }));
}
function parseComponents(input: string, segments: readonly Segment[], locale: EditorLocale): { parts: DateParts; seconds: boolean } | undefined {
  const directives: { code: Directive; names?: string[] }[] = [];
  const expression = segments.map(segment => {
    if (segment.kind === "literal") return escaped(segment.value).replace(/\s+/g, "\\s+");
    const code = segment.code;
    const named = ["p", "a", "A", "b", "B"].includes(code);
    const names = named ? nameValues(code, locale) : undefined;
    directives.push({ code, ...(names ? { names } : {}) });
    return names ? `(${[...names].sort((a, b) => b.length - a.length).map(escaped).join("|")})` : code === "Y" ? "([0-9]{4})" : "([0-9]{1,2})";
  }).join("");
  const match = new RegExp(`^${expression}$`, "iu").exec(input);
  if (!match) return undefined;
  const parts: DateParts = { year: 2000, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
  let twelve: number | undefined, meridiem: number | undefined, weekday: number | undefined;
  const written = new Map<string, number>();
  const set = (key: keyof DateParts, value: number): boolean => {
    if (written.has(key) && written.get(key) !== value) return false;
    written.set(key, value); parts[key] = value; return true;
  };
  for (let index = 0; index < directives.length; index++) {
    const { code, names } = directives[index]!, raw = match[index + 1]!;
    const number = names ? names.findIndex(name => name.toLocaleLowerCase(language(locale)) === raw.toLocaleLowerCase(language(locale))) : Number(raw);
    if (names && number < 0) return undefined;
    switch (code) {
      case "Y": if (!set("year", number)) return undefined; break;
      case "y": if (!set("year", number > 60 ? 1900 + number : 2000 + number)) return undefined; break;
      case "m": if (!set("month", number)) return undefined; break;
      case "b": case "B": if (!set("month", number + 1)) return undefined; break;
      case "d": if (!set("day", number)) return undefined; break;
      case "H": if (!set("hour", number)) return undefined; break;
      case "I": twelve = number; break;
      case "M": if (!set("minute", number)) return undefined; break;
      case "S": if (!set("second", number)) return undefined; break;
      case "p": meridiem = number; break;
      case "a": case "A": weekday = number + 1; break;
    }
  }
  if (twelve !== undefined) {
    if (twelve < 1 || twelve > 12 || meridiem === undefined) return undefined;
    parts.hour = twelve % 12 + meridiem * 12;
  }
  const value = utcDate(parts);
  if (!value || weekday !== undefined && value.weekday !== weekday) return undefined;
  return { parts, seconds: directives.some(directive => directive.code === "S") };
}

/** Parse a user draft. Conversion errors are returned; malformed owner configuration throws. */
export function parseEditorValue(kind: DateKind, input: string, locale: EditorLocale, options: DateParseOptions = {}): ParseResult {
  dateKind(kind); checkedOptions(options, ["previous", "showSeconds"]); const configured = editorLocaleFor(kind, locale); bool(options.showSeconds);
  const previous = options.previous === undefined ? null : canonical(kind, options.previous, configured);
  const bounded = boundedInput(input); if (bounded) return bounded;
  const raw = input.trim(); if (!raw) return { ok: true, value: false };
  const showSeconds = options.showSeconds ?? true;
  if (previous && raw === render(previous, dateTimeFormat(kind, configured, showSeconds), configured)) return { ok: true, value: options.previous! };
  if (kind === "date") {
    const iso = raw.match(DATE_WIRE);
    const parsed = iso ? { parts: { year: Number(iso[1]), month: Number(iso[2]), day: Number(iso[3]), hour: 0, minute: 0, second: 0 }, seconds: false } : parseComponents(raw, dateTimeFormat(kind, configured, showSeconds), configured);
    if (!parsed) return fail(messageFor(kind));
    return valueFromParts(kind, parsed.parts, configured, options.previous);
  }
  const iso = raw.match(ISO_INPUT);
  let parsed: { parts: DateParts; seconds: boolean } | undefined;
  if (iso) {
    parsed = { parts: { year: Number(iso[1]), month: Number(iso[2]), day: Number(iso[3]), hour: Number(iso[4]), minute: Number(iso[5]), second: Number(iso[6] ?? 0) }, seconds: iso[6] !== undefined };
    if (!utcDate(parsed.parts)) return fail(messageFor(kind));
    if (iso[7]) {
      const offset = iso[7];
      if (offset.toUpperCase() !== "Z" && (Number(offset.slice(1, 3)) > 23 || Number(offset.slice(4, 6)) > 59)) return fail(messageFor(kind));
      let absolute = DateTime.fromISO(raw.replace(" ", "T"), { setZone: true });
      if (!absolute.isValid || absolute.toUTC().year < 1 || absolute.toUTC().year > 9999) return fail(messageFor(kind));
      if (!parsed.seconds && !showSeconds && previous) absolute = absolute.set({ second: previous.second });
      return { ok: true, value: wire(absolute, "datetime") };
    }
  } else parsed = parseComponents(raw, dateTimeFormat(kind, configured, showSeconds), configured);
  if (!parsed) return fail(messageFor(kind));
  if (!parsed.seconds && !showSeconds && previous) parsed.parts.second = previous.second;
  return localValue(parsed.parts, configured, options.previous);
}

export function formatTimeValue(value: EditorValue, locale: EditorLocale, showSeconds = false): string {
  const configured = editorLocaleFor("time", locale); bool(showSeconds); const parsed = canonicalTime(value);
  return parsed ? render(parsed, timeFormat(configured, showSeconds), configured) : "";
}
function roundingOption(rounding: number): void {
  if (!Number.isInteger(rounding) || rounding < 0 || rounding > 60) throw new ClientError("validation", "Time rounding must be an integer from 0 to 60 minutes.");
}
export function roundTimeValue(value: string, rounding: number): string {
  roundingOption(rounding);
  if (typeof value !== "string") throw new ClientError("server", "Invalid represented time value.");
  const parsed = canonicalTime(value)!;
  const minute = rounding <= 1 ? parsed.minute : Math.round(parsed.minute / rounding) * rounding;
  const bounded = (parsed.hour * 60 + minute) % 1440;
  return `${String(Math.floor(bounded / 60)).padStart(2, "0")}:${String(bounded % 60).padStart(2, "0")}:${String(parsed.second).padStart(2, "0")}`;
}
export function parseTimeValue(input: string, locale: EditorLocale, options: TimeParseOptions = {}): ParseResult {
  checkedOptions(options, ["previous", "showSeconds", "rounding"]);
  const configured = editorLocaleFor("time", locale); bool(options.showSeconds);
  if (options.rounding !== undefined) roundingOption(options.rounding);
  roundingOption(options.rounding ?? 0);
  const previous = options.previous === undefined ? null : canonicalTime(options.previous);
  const bounded = boundedInput(input); if (bounded) return bounded;
  const raw = input.trim(); if (!raw) return { ok: true, value: false };
  const native = raw.match(NATIVE_TIME);
  const showSeconds = options.showSeconds ?? false;
  const parsed = native ? { parts: { year: 2000, month: 1, day: 1, hour: Number(native[1]), minute: Number(native[2]), second: Number(native[3] ?? 0) }, seconds: native[3] !== undefined } : parseComponents(raw, timeFormat(configured, showSeconds), configured);
  if (!parsed || !utcDate(parsed.parts)) return fail("Enter a valid time.");
  if (!parsed.seconds && !showSeconds && previous) parsed.parts.second = previous.second;
  const value = `${String(parsed.parts.hour).padStart(2, "0")}:${String(parsed.parts.minute).padStart(2, "0")}:${String(parsed.parts.second).padStart(2, "0")}`;
  return { ok: true, value: roundTimeValue(value, options.rounding ?? 0) };
}
/** At most 240 desktop suggestions; rounding <=5 uses quarter-hour suggestions. */
export function timeSuggestions(rounding = 5): readonly string[] {
  roundingOption(rounding);
  const step = rounding <= 5 ? 15 : rounding, values: string[] = [];
  for (let hour = 0; hour < 24; hour++) for (let minute = 0; minute < 60; minute += step) values.push(`${String(hour).padStart(2, "0")}:${String(minute).padStart(2, "0")}:00`);
  return values;
}

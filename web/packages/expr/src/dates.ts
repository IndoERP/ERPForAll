/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { type Sig, bindSig, intArg, requiredSlot, sig0, strArg } from "./args";
import { type Budget, utf8ByteLength } from "./budget";
import {
  type PythonError,
  attributeError,
  indexError,
  moduleAttributeError,
  overflowError,
  typeAttributeError,
  typeError,
  valueError,
  zeroDivision,
} from "./python-error";
import {
  type DateValue,
  type DateTimeValue,
  type ExprObject,
  type ExprValue,
  type FloatValue,
  type RelativeDeltaValue,
  type TimeDeltaValue,
  type Weekday,
  callableVal,
  dateTimeVal,
  dateVal,
  floatVal,
  intVal,
  pythonTypeOf,
  repr,
  strVal,
} from "./values";

export const MIN_YEAR = 1;
export const MAX_YEAR = 9999;
export const MAX_DAYS = 999_999_999n;

export const MICROS_PER_SECOND = 1_000_000n;
export const MICROS_PER_DAY = 86_400n * MICROS_PER_SECOND;

export function dateRangeError(): PythonError {
  return overflowError("date value out of range");
}

const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;
const I128_MIN = -(1n << 127n);
const I128_MAX = (1n << 127n) - 1n;

/** A checked i64 result (Rust `checked_*` on `i64`): out of range is `OverflowError: date value out of range`. */
function i64(n: bigint): bigint {
  if (n < I64_MIN || n > I64_MAX) {
    throw dateRangeError();
  }
  return n;
}

/** A checked i128 result (Rust `checked_*` on `i128`). */
function i128(n: bigint): bigint {
  if (n < I128_MIN || n > I128_MAX) {
    throw dateRangeError();
  }
  return n;
}

/** Euclidean remainder (Rust `rem_euclid`). */
function remEuclid(a: number, b: number): number {
  const r = a % b;
  return r < 0 ? r + Math.abs(b) : r;
}

/** The integer a float holds exactly, when it holds one in the i64 range (Rust `num::float_as_int`). */
export function floatAsInt(f: number): bigint | null {
  // 2**63 is exact as a double; every finite double below it with no fraction converts exactly.
  const LIMIT = 9_223_372_036_854_775_808;
  if (Number.isFinite(f) && Number.isInteger(f) && f >= -LIMIT && f < LIMIT) {
    return BigInt(f);
  }
  return null;
}

export function isLeapYear(year: number): boolean {
  return (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
}

const MONTH_DAYS = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

export function daysInMonth(year: number, month: number): number {
  if (month < 1 || month > 12) {
    throw valueError("month must be in 1..12");
  }
  if (month === 2 && isLeapYear(year)) {
    return 29;
  }
  return MONTH_DAYS[month]!;
}

export interface CalendarDate {
  readonly year: number;
  readonly month: number;
  readonly day: number;
}

export interface ClockTime {
  readonly hour: number;
  readonly minute: number;
  readonly second: number;
  readonly microsecond: number;
}

const MIDNIGHT: ClockTime = Object.freeze({ hour: 0, minute: 0, second: 0, microsecond: 0 });

/**
 * A Python-valid date from i64 fields (Rust `datetime::make_date`): the year, then the month, then the
 * day are checked; messages print the exact i64.
 */
export function makeDate(year: bigint, month: bigint, day: bigint): CalendarDate {
  if (year < BigInt(MIN_YEAR) || year > BigInt(MAX_YEAR)) {
    throw valueError(`year ${year} is out of range`);
  }
  if (month < 1n || month > 12n) {
    throw valueError("month must be in 1..12");
  }
  const y = Number(year);
  const m = Number(month);
  if (day < 1n || day > BigInt(daysInMonth(y, m))) {
    throw valueError("day is out of range for month");
  }
  return { year: y, month: m, day: Number(day) };
}

/** A valid time from i64 fields (Rust `datetime::make_time`). */
export function makeTime(hour: bigint, minute: bigint, second: bigint, micro: bigint): ClockTime {
  const field = (v: bigint, max: bigint, name: string): number => {
    if (v < 0n || v > max) {
      throw valueError(`${name} must be in 0..${max}`);
    }
    return Number(v);
  };
  const h = field(hour, 23n, "hour");
  const m = field(minute, 59n, "minute");
  const s = field(second, 59n, "second");
  if (micro < 0n || micro > 999_999n) {
    throw valueError("microsecond must be in 0..999999");
  }
  return { hour: h, minute: m, second: s, microsecond: Number(micro) };
}

// Proleptic Gregorian day number (0 = 0001-01-01)
export function toDays(year: number, month: number, day: number): number {
  const a = Math.floor((14 - month) / 12);
  const y = year + 4800 - a;
  const m = month + 12 * a - 3;
  return (
    day +
    Math.floor((153 * m + 2) / 5) +
    365 * y +
    Math.floor(y / 4) -
    Math.floor(y / 100) +
    Math.floor(y / 400) -
    32045 -
    1721426 // Epoch offset so 0001-01-01 is day 0
  );
}

export function fromDays(days: number): { year: number; month: number; day: number } {
  const jdn = days + 1721426;
  const a = jdn + 32044;
  const b = Math.floor((4 * a + 3) / 146097);
  const c = a - Math.floor((146097 * b) / 4);
  const d = Math.floor((4 * c + 3) / 1461);
  const e = c - Math.floor((1461 * d) / 4);
  const m = Math.floor((5 * e + 2) / 153);
  const day = e - Math.floor((153 * m + 2) / 5) + 1;
  const month = m < 10 ? m + 3 : m - 9;
  const year = 100 * b + d - 4800 + Math.floor((m + 2) / 12);
  if (year < MIN_YEAR || year > MAX_YEAR) {
    throw dateRangeError();
  }
  return { year, month, day };
}

export function addDaysToCalendar(
  year: number,
  month: number,
  day: number,
  daysDelta: number,
): { year: number; month: number; day: number } {
  if (Math.abs(daysDelta) > 4_000_000) {
    throw dateRangeError();
  }
  const currentDays = toDays(year, month, day);
  return fromDays(currentDays + daysDelta);
}

/** `date + days` (Rust `datetime::add_days`): beyond 4,000,000 days or outside 1..9999 is an `OverflowError`. */
function addDays(date: CalendarDate, days: bigint): CalendarDate {
  if (days > 4_000_000n || days < -4_000_000n) {
    throw dateRangeError();
  }
  return addDaysToCalendar(date.year, date.month, date.day, Number(days));
}

// Monday = 0, Sunday = 6
export function weekdayOf(year: number, month: number, day: number): number {
  const d = toDays(year, month, day);
  // 0001-01-01 was a Monday (day 0 -> weekday 0)
  return ((d % 7) + 7) % 7;
}

// Day of year: 1..366
export function dayOfYear(year: number, month: number, day: number): number {
  let ord = day;
  for (let m = 1; m < month; m++) {
    ord += daysInMonth(year, m);
  }
  return ord;
}

// -------------------------------------------------------------------------------------------------
// TimeDelta
// -------------------------------------------------------------------------------------------------

export function normalizeTimeDelta(totalMicros: bigint): TimeDeltaValue {
  let days = totalMicros / MICROS_PER_DAY;
  let rem = totalMicros % MICROS_PER_DAY;
  if (rem < 0n) {
    rem += MICROS_PER_DAY;
    days -= 1n;
  }
  if (days < -MAX_DAYS || days > MAX_DAYS) {
    throw overflowError(`days=${days}; must have magnitude <= ${MAX_DAYS}`);
  }
  const seconds = Number(rem / MICROS_PER_SECOND);
  const microseconds = Number(rem % MICROS_PER_SECOND);
  return Object.freeze({
    kind: "timedelta",
    days: Number(days),
    seconds,
    microseconds,
  });
}

export function timeDeltaTotalMicros(td: TimeDeltaValue): bigint {
  return (
    BigInt(td.days) * MICROS_PER_DAY +
    BigInt(td.seconds) * MICROS_PER_SECOND +
    BigInt(td.microseconds)
  );
}

/** Rust `datetime::unit_micros`: `value * unit` rounded half to even, bounded to 1e30. */
function roundTiesEven(x: number): bigint {
  if (!Number.isFinite(x) || Math.abs(x) > 1e30) {
    throw overflowError("timedelta value out of range");
  }
  const floor = Math.floor(x);
  const diff = x - floor;
  if (diff > 0.5) {
    return BigInt(floor + 1);
  }
  if (diff < 0.5) {
    return BigInt(floor);
  }
  // Exactly 0.5: round to even integer
  return BigInt(floor % 2 === 0 ? floor : floor + 1);
}

/** A number argument of `timedelta` in microseconds of `unit` (Rust `micros_of`). */
function microsOf(fname: string, param: string, v: ExprValue, unit: bigint): bigint {
  if (v.kind === "int" || v.kind === "bool") {
    return i128(intArg(fname, param, v) * unit);
  }
  if (v.kind === "float") {
    return roundTiesEven(v.value * Number(unit));
  }
  throw typeError(`unsupported type for ${fname} ${param} component: ${pythonTypeOf(v)}`);
}

const TD_MICRO = 1n;
const TD_MILLI = 1_000n;
const TD_SECOND = 1_000_000n;
const TD_MINUTE = 60n * TD_SECOND;
const TD_HOUR = 60n * TD_MINUTE;
const TD_DAY = 24n * TD_HOUR;
const TD_WEEK = 7n * TD_DAY;

const TD_PARAMS = ["days", "seconds", "microseconds", "milliseconds", "minutes", "hours", "weeks"] as const;
const TD_UNITS = [TD_DAY, TD_SECOND, TD_MICRO, TD_MILLI, TD_MINUTE, TD_HOUR, TD_WEEK];

const TD_SIG: Sig = { name: "timedelta", params: TD_PARAMS, required: 0, posOnly: 0, maxPos: 7 };

/** `timedelta(...)`: arguments are bound first, then converted in parameter order (Rust `timedelta`). */
export function createTimeDelta(args: readonly ExprValue[], kwargs: Record<string, ExprValue>): TimeDeltaValue {
  const slots = bindSig(TD_SIG, args, kwargs);
  let totalMicros = 0n;
  for (let i = 0; i < TD_PARAMS.length; i++) {
    const v = slots[i];
    if (v !== undefined) {
      totalMicros = i128(totalMicros + microsOf("timedelta", TD_PARAMS[i]!, v, TD_UNITS[i]!));
    }
  }
  return normalizeTimeDelta(totalMicros);
}

export function timeDeltaAdd(a: TimeDeltaValue, b: TimeDeltaValue): TimeDeltaValue {
  return normalizeTimeDelta(timeDeltaTotalMicros(a) + timeDeltaTotalMicros(b));
}

export function timeDeltaSub(a: TimeDeltaValue, b: TimeDeltaValue): TimeDeltaValue {
  return normalizeTimeDelta(timeDeltaTotalMicros(a) - timeDeltaTotalMicros(b));
}

export function timeDeltaNeg(td: TimeDeltaValue): TimeDeltaValue {
  return normalizeTimeDelta(-timeDeltaTotalMicros(td));
}

export function timeDeltaAbs(td: TimeDeltaValue): TimeDeltaValue {
  const m = timeDeltaTotalMicros(td);
  return normalizeTimeDelta(m < 0n ? -m : m);
}

export function timeDeltaMul(td: TimeDeltaValue, factor: ExprValue): TimeDeltaValue {
  const m = timeDeltaTotalMicros(td);
  if (factor.kind === "int") {
    // Rust multiplies the i128 microseconds with `checked_mul` (ops.rs `mul`).
    return normalizeTimeDelta(i128(m * factor.value));
  }
  if (factor.kind === "bool") {
    return normalizeTimeDelta(factor.value ? m : 0n);
  }
  if (factor.kind === "float") {
    const scaled = Number(m) * factor.value;
    return normalizeTimeDelta(roundTiesEven(scaled));
  }
  throw typeError(`unsupported operand type(s) for *: 'datetime.timedelta' and '${pythonTypeOf(factor)}'`);
}

export function timeDeltaDiv(a: TimeDeltaValue, b: TimeDeltaValue): FloatValue {
  const mb = timeDeltaTotalMicros(b);
  if (mb === 0n) {
    throw zeroDivision("division by zero");
  }
  const ma = timeDeltaTotalMicros(a);
  return floatVal(Number(ma) / Number(mb));
}

export function timeDeltaTotalSeconds(td: TimeDeltaValue): number {
  return Number(timeDeltaTotalMicros(td)) / 1e6;
}

export function compareTimeDeltas(a: TimeDeltaValue, b: TimeDeltaValue): number {
  const ma = timeDeltaTotalMicros(a);
  const mb = timeDeltaTotalMicros(b);
  if (ma < mb) return -1;
  if (ma > mb) return 1;
  return 0;
}

// -------------------------------------------------------------------------------------------------
// Date and DateTime
// -------------------------------------------------------------------------------------------------

const DATETIME_SIG: Sig = {
  name: "datetime",
  params: ["year", "month", "day", "hour", "minute", "second", "microsecond"],
  required: 3,
  posOnly: 0,
  maxPos: 7,
};

/** `datetime(...)`: the date is converted and checked before the time fields (Rust `DateTimeClass::call`). */
export function createDateTime(args: readonly ExprValue[], kwargs: Record<string, ExprValue>): DateTimeValue {
  const sig = DATETIME_SIG;
  const slots = bindSig(sig, args, kwargs);
  const field = (i: number): bigint => {
    const v = slots[i];
    return v === undefined ? 0n : intArg("datetime", sig.params[i]!, v);
  };
  const date = makeDate(
    intArg("datetime", "year", requiredSlot(sig, slots, 0)),
    intArg("datetime", "month", requiredSlot(sig, slots, 1)),
    intArg("datetime", "day", requiredSlot(sig, slots, 2)),
  );
  const hour = field(3);
  const minute = field(4);
  const second = field(5);
  const micro = field(6);
  const time = makeTime(hour, minute, second, micro);
  return dateTimeVal(date.year, date.month, date.day, time.hour, time.minute, time.second, time.microsecond);
}

export function dateAddTimeDelta(d: DateValue, td: TimeDeltaValue): DateValue {
  const { year, month, day } = addDaysToCalendar(d.year, d.month, d.day, td.days);
  return Object.freeze({ kind: "date", year, month, day });
}

export function dateTimeAddTimeDelta(dt: DateTimeValue, td: TimeDeltaValue): DateTimeValue {
  const currentMicros =
    BigInt(dt.hour) * 3600n * MICROS_PER_SECOND +
    BigInt(dt.minute) * 60n * MICROS_PER_SECOND +
    BigInt(dt.second) * MICROS_PER_SECOND +
    BigInt(dt.microsecond);
  const deltaMicros =
    BigInt(td.seconds) * MICROS_PER_SECOND + BigInt(td.microseconds);
  const totalMicros = currentMicros + deltaMicros;

  let extraDays = totalMicros / MICROS_PER_DAY;
  let rem = totalMicros % MICROS_PER_DAY;
  if (rem < 0n) {
    rem += MICROS_PER_DAY;
    extraDays -= 1n;
  }
  const daysDelta = td.days + Number(extraDays);
  const { year, month, day } = addDaysToCalendar(dt.year, dt.month, dt.day, daysDelta);

  const sec = Number(rem / MICROS_PER_SECOND);
  const micro = Number(rem % MICROS_PER_SECOND);
  const hour = Math.floor(sec / 3600);
  const minute = Math.floor((sec % 3600) / 60);
  const second = sec % 60;

  return Object.freeze({
    kind: "datetime",
    year,
    month,
    day,
    hour,
    minute,
    second,
    microsecond: micro,
  });
}

export function dateSubDate(a: DateValue, b: DateValue): TimeDeltaValue {
  const da = toDays(a.year, a.month, a.day);
  const db = toDays(b.year, b.month, b.day);
  const diffDays = BigInt(da - db);
  return normalizeTimeDelta(diffDays * MICROS_PER_DAY);
}

export function dateTimeSubDateTime(a: DateTimeValue, b: DateTimeValue): TimeDeltaValue {
  const da = toDays(a.year, a.month, a.day);
  const db = toDays(b.year, b.month, b.day);
  const diffDays = BigInt(da - db);
  const ma =
    BigInt(a.hour) * 3600n * MICROS_PER_SECOND +
    BigInt(a.minute) * 60n * MICROS_PER_SECOND +
    BigInt(a.second) * MICROS_PER_SECOND +
    BigInt(a.microsecond);
  const mb =
    BigInt(b.hour) * 3600n * MICROS_PER_SECOND +
    BigInt(b.minute) * 60n * MICROS_PER_SECOND +
    BigInt(b.second) * MICROS_PER_SECOND +
    BigInt(b.microsecond);
  const total = diffDays * MICROS_PER_DAY + (ma - mb);
  return normalizeTimeDelta(total);
}

export function compareDates(
  a: { year: number; month: number; day: number },
  b: { year: number; month: number; day: number },
): number {
  if (a.year !== b.year) return a.year < b.year ? -1 : 1;
  if (a.month !== b.month) return a.month < b.month ? -1 : 1;
  if (a.day !== b.day) return a.day < b.day ? -1 : 1;
  return 0;
}

export function compareDateTimes(a: DateTimeValue, b: DateTimeValue): number {
  const cd = compareDates(a, b);
  if (cd !== 0) return cd;
  if (a.hour !== b.hour) return a.hour < b.hour ? -1 : 1;
  if (a.minute !== b.minute) return a.minute < b.minute ? -1 : 1;
  if (a.second !== b.second) return a.second < b.second ? -1 : 1;
  if (a.microsecond !== b.microsecond) return a.microsecond < b.microsecond ? -1 : 1;
  return 0;
}

const DAY_NAMES = [
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
  "Sunday",
];

const MONTH_NAMES = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
];

export function dateIso(d: { year: number; month: number; day: number }): string {
  const y = String(d.year).padStart(4, "0");
  const m = String(d.month).padStart(2, "0");
  const day = String(d.day).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

export function dateTimeIso(
  dt: { year: number; month: number; day: number; hour: number; minute: number; second: number; microsecond: number },
  sep: string = "T",
): string {
  const base = `${dateIso(dt)}${sep}${String(dt.hour).padStart(2, "0")}:${String(dt.minute).padStart(2, "0")}:${String(dt.second).padStart(2, "0")}`;
  if (dt.microsecond !== 0) {
    return `${base}.${String(dt.microsecond).padStart(6, "0")}`;
  }
  return base;
}

/**
 * Python `strftime` (C locale) for a naive date/time (Rust `datetime::strftime`). The format is
 * scanned and the output built in a capped buffer charged to the budget; directives are read by
 * Unicode scalar, so an unsupported one is named whole.
 */
export function strftime(date: CalendarDate, time: ClockTime, fmt: string, budget: Budget): string {
  budget.scan(utf8ByteLength(fmt));
  const out = budget.out();
  const chars = fmt[Symbol.iterator]();
  const weekday = weekdayOf(date.year, date.month, date.day);
  const dayName = DAY_NAMES[weekday]!;
  const monthName = MONTH_NAMES[date.month - 1]!;
  const hour12 = time.hour % 12 === 0 ? 12 : time.hour % 12;
  const ord = dayOfYear(date.year, date.month, date.day);

  for (let next = chars.next(); !next.done; next = chars.next()) {
    const c = next.value;
    if (c !== "%") {
      out.push(c);
      continue;
    }
    const directive = chars.next();
    if (directive.done) {
      throw valueError("stray % at the end of the strftime format");
    }
    const d = directive.value;
    out.push(strftimePiece(d, date, time, weekday, dayName, monthName, hour12, ord));
  }
  return budget.finish(out);
}

function strftimePiece(
  d: string,
  date: CalendarDate,
  time: ClockTime,
  weekday: number,
  dayName: string,
  monthName: string,
  hour12: number,
  ord: number,
): string {
  let out = "";
  switch (d) {
    case "Y":
      // Rust `date.year().to_string()`: no zero padding (only %F pads, through the ISO form).
      out += String(date.year);
      break;
    case "y":
      out += String(((date.year % 100) + 100) % 100).padStart(2, "0");
      break;
    case "m":
      out += String(date.month).padStart(2, "0");
      break;
    case "d":
      out += String(date.day).padStart(2, "0");
      break;
    case "e":
      out += String(date.day).padStart(2, " ");
      break;
    case "H":
      out += String(time.hour).padStart(2, "0");
      break;
    case "I":
      out += String(hour12).padStart(2, "0");
      break;
    case "M":
      out += String(time.minute).padStart(2, "0");
      break;
    case "S":
      out += String(time.second).padStart(2, "0");
      break;
    case "f":
      out += String(time.microsecond).padStart(6, "0");
      break;
    case "p":
      out += time.hour < 12 ? "AM" : "PM";
      break;
    case "j":
      out += String(ord).padStart(3, "0");
      break;
    case "w": {
      // Sunday = 0, Monday = 1, ..., Saturday = 6
      const sundayBased = (weekday + 1) % 7;
      out += String(sundayBased);
      break;
    }
    case "u":
      // Monday = 1, ..., Sunday = 7
      out += String(weekday + 1);
      break;
    case "a":
      out += dayName.slice(0, 3);
      break;
    case "A":
      out += dayName;
      break;
    case "b":
    case "h":
      out += monthName.slice(0, 3);
      break;
    case "B":
      out += monthName;
      break;
    case "U": {
      // Sunday-based week of the year (00..53). All days in a new year preceding the first Sunday are in week 0.
      const jan1SundayBased = (weekdayOf(date.year, 1, 1) + 1) % 7;
      const firstSundayOffset = (7 - jan1SundayBased) % 7;
      const dayOfYear0 = ord - 1;
      let week = 0;
      if (dayOfYear0 >= firstSundayOffset) {
        week = Math.floor((dayOfYear0 - firstSundayOffset) / 7) + 1;
      }
      out += String(week).padStart(2, "0");
      break;
    }
    case "W": {
      // Monday-based week of the year (00..53). All days in a new year preceding the first Monday are in week 0.
      const jan1MondayBased = weekdayOf(date.year, 1, 1);
      const firstMondayOffset = (7 - jan1MondayBased) % 7;
      const dayOfYear0 = ord - 1;
      let week = 0;
      if (dayOfYear0 >= firstMondayOffset) {
        week = Math.floor((dayOfYear0 - firstMondayOffset) / 7) + 1;
      }
      out += String(week).padStart(2, "0");
      break;
    }
    case "V": {
      // ISO 8601 week number (01..53)
      // Find Thursday of current week:
      const thursdayOffset = 3 - weekday;
      const currentDays = toDays(date.year, date.month, date.day);
      const thursdayDate = fromDays(currentDays + thursdayOffset);
      const isoYear = thursdayDate.year;
      const jan4Days = toDays(isoYear, 1, 4);
      const jan4Weekday = weekdayOf(isoYear, 1, 4);
      const jan4Monday = jan4Days - jan4Weekday;
      const thursdayDays = toDays(thursdayDate.year, thursdayDate.month, thursdayDate.day);
      const isoWeek = Math.floor((thursdayDays - jan4Monday) / 7) + 1;
      out += String(isoWeek).padStart(2, "0");
      break;
    }
    case "G": {
      const thursdayOffset = 3 - weekday;
      const currentDays = toDays(date.year, date.month, date.day);
      const thursdayDate = fromDays(currentDays + thursdayOffset);
      out += String(thursdayDate.year);
      break;
    }
    case "z":
    case "Z":
      break;
    case "c":
      out += `${dayName.slice(0, 3)} ${monthName.slice(0, 3)} ${String(date.day).padStart(2, " ")} ${String(time.hour).padStart(2, "0")}:${String(time.minute).padStart(2, "0")}:${String(time.second).padStart(2, "0")} ${date.year}`;
      break;
    case "x":
    case "D":
      out += `${String(date.month).padStart(2, "0")}/${String(date.day).padStart(2, "0")}/${String(((date.year % 100) + 100) % 100).padStart(2, "0")}`;
      break;
    case "X":
    case "T":
      out += `${String(time.hour).padStart(2, "0")}:${String(time.minute).padStart(2, "0")}:${String(time.second).padStart(2, "0")}`;
      break;
    case "F":
      out += dateIso(date);
      break;
    case "%":
      out += "%";
      break;
    default:
      throw valueError(`unsupported strftime directive '%${d}'`);
  }
  return out;
}

// -------------------------------------------------------------------------------------------------
// RelativeDelta
// -------------------------------------------------------------------------------------------------

/** The relative (plural) fields of a relativedelta, mutable while it is built. */
interface RelativeFields {
  years: bigint;
  months: bigint;
  days: bigint;
  leapdays: bigint;
  hours: bigint;
  minutes: bigint;
  seconds: bigint;
  microseconds: bigint;
}

/** dateutil's carry of one field into the next larger one, preserving the sign (Rust `carry`). */
function carry(small: bigint, large: bigint, base: bigint, limit: bigint): [bigint, bigint] {
  const abs = small < 0n ? -small : small;
  if (abs <= limit) {
    return [small, large];
  }
  const s = small < 0n ? -1n : 1n;
  const magnitude = i64(small * s);
  const div = magnitude / base;
  const rem = magnitude % base;
  return [rem * s, i64(large + i64(div * s))];
}

/** dateutil's `_fix`: carries overflowing relative fields upward (Rust `RelativeDelta::normalize`). */
export function normalizeRelativeDelta(rd: RelativeFields): void {
  [rd.microseconds, rd.seconds] = carry(rd.microseconds, rd.seconds, 1_000_000n, 999_999n);
  [rd.seconds, rd.minutes] = carry(rd.seconds, rd.minutes, 60n, 59n);
  [rd.minutes, rd.hours] = carry(rd.minutes, rd.hours, 60n, 59n);
  [rd.hours, rd.days] = carry(rd.hours, rd.days, 24n, 23n);
  [rd.months, rd.years] = carry(rd.months, rd.years, 12n, 11n);
}

export function relativeDeltaHasTime(rd: RelativeDeltaValue): boolean {
  return (
    rd.hours !== 0n ||
    rd.minutes !== 0n ||
    rd.seconds !== 0n ||
    rd.microseconds !== 0n ||
    rd.hour !== null ||
    rd.minute !== null ||
    rd.second !== null ||
    rd.microsecond !== null
  );
}

export function relativeDeltaIsZero(rd: RelativeDeltaValue): boolean {
  return (
    rd.years === 0n &&
    rd.months === 0n &&
    rd.days === 0n &&
    rd.leapdays === 0n &&
    rd.hours === 0n &&
    rd.minutes === 0n &&
    rd.seconds === 0n &&
    rd.microseconds === 0n &&
    rd.year === null &&
    rd.month === null &&
    rd.day === null &&
    rd.weekday === null &&
    rd.hour === null &&
    rd.minute === null &&
    rd.second === null &&
    rd.microsecond === null
  );
}

const RD_UNITS: Readonly<Record<string, bigint>> = Object.freeze({
  weeks: TD_WEEK,
  days: TD_DAY,
  hours: TD_HOUR,
  minutes: TD_MINUTE,
  seconds: TD_SECOND,
  microseconds: TD_MICRO,
});

/** `relativedelta(**kwargs)` (Rust `builtins::dates::relativedelta`). */
export function createRelativeDelta(args: readonly ExprValue[], kwargs: Record<string, ExprValue>): RelativeDeltaValue {
  if (args.length > 0) {
    throw typeError("relativedelta(dt1, dt2) is not supported in expressions; pass keyword arguments");
  }
  const rel: RelativeFields = {
    years: 0n,
    months: 0n,
    days: 0n,
    leapdays: 0n,
    hours: 0n,
    minutes: 0n,
    seconds: 0n,
    microseconds: 0n,
  };
  let year: bigint | null = null;
  let month: bigint | null = null;
  let day: bigint | null = null;
  let weekday: Weekday | null = null;
  let hour: bigint | null = null;
  let minute: bigint | null = null;
  let second: bigint | null = null;
  let microsecond: bigint | null = null;
  let floatMicros = 0n;

  for (const key of Object.keys(kwargs)) {
    const v = kwargs[key]!;
    const int = (x: ExprValue): bigint => {
      if (x.kind === "float") {
        const i = floatAsInt(x.value);
        if (i === null) {
          throw valueError(`relativedelta argument '${key}' must be an integer, got ${repr(x)}`);
        }
        return i;
      }
      return intArg("relativedelta", key, x);
    };
    switch (key) {
      case "years":
      case "months": {
        if (v.kind === "float" && floatAsInt(v.value) === null) {
          throw valueError("Non-integer years and months are ambiguous and not currently supported.");
        }
        const x = int(v);
        if (key === "years") rel.years = x;
        else rel.months = x;
        break;
      }
      case "leapdays":
        rel.leapdays = int(v);
        break;
      case "weeks":
      case "days":
      case "hours":
      case "minutes":
      case "seconds":
      case "microseconds": {
        if (v.kind === "float" && floatAsInt(v.value) === null) {
          // dateutil keeps fractional values; adding them equals adding the equivalent microseconds.
          floatMicros = i128(floatMicros + roundTiesEven(v.value * Number(RD_UNITS[key]!)));
          break;
        }
        const x = int(v);
        if (key === "weeks") {
          rel.days = i64(rel.days + i64(x * 7n));
        } else if (key === "days") {
          rel.days = i64(rel.days + x);
        } else if (key === "hours") {
          rel.hours = x;
        } else if (key === "minutes") {
          rel.minutes = x;
        } else if (key === "seconds") {
          rel.seconds = x;
        } else {
          rel.microseconds = x;
        }
        break;
      }
      case "year":
        year = int(v);
        break;
      case "month": {
        const m = int(v);
        if (m < 1n || m > 12n) throw valueError("month must be in 1..12");
        month = m;
        break;
      }
      case "day": {
        const d = int(v);
        if (d < 1n || d > 31n) throw valueError("day must be in 1..31");
        day = d;
        break;
      }
      case "hour":
        hour = int(v);
        break;
      case "minute":
        minute = int(v);
        break;
      case "second":
        second = int(v);
        break;
      case "microsecond":
        microsecond = int(v);
        break;
      case "weekday": {
        const w = int(v);
        let wd: bigint;
        if (w >= 0n && w <= 6n) wd = w;
        else if (w >= -7n && w <= -1n) wd = w + 7n;
        else throw indexError("tuple index out of range");
        weekday = Object.freeze({ day: Number(wd), n: null });
        break;
      }
      case "yearday":
      case "nlyearday":
        throw valueError(`relativedelta(${key}=...) is not supported in expressions`);
      default:
        throw typeError(`relativedelta.__init__() got an unexpected keyword argument '${key}'`);
    }
  }

  if (floatMicros !== 0n) {
    rel.microseconds = i64(rel.microseconds + i64(floatMicros));
  }
  normalizeRelativeDelta(rel);

  return Object.freeze({
    kind: "relativedelta",
    ...rel,
    year,
    month,
    day,
    weekday,
    hour,
    minute,
    second,
    microsecond,
  });
}

/** `-rd`: relative fields negated (checked), absolute fields kept (Rust `RelativeDelta::negated`). */
export function relativeDeltaNeg(rd: RelativeDeltaValue): RelativeDeltaValue {
  return Object.freeze({
    ...rd,
    years: i64(-rd.years),
    months: i64(-rd.months),
    days: i64(-rd.days),
    leapdays: i64(-rd.leapdays),
    hours: i64(-rd.hours),
    minutes: i64(-rd.minutes),
    seconds: i64(-rd.seconds),
    microseconds: i64(-rd.microseconds),
  });
}

/** `a + b`: relative fields add (checked), `b`'s absolute fields win (Rust `RelativeDelta::plus`). */
export function relativeDeltaPlus(a: RelativeDeltaValue, b: RelativeDeltaValue): RelativeDeltaValue {
  const rel: RelativeFields = {
    years: i64(a.years + b.years),
    months: i64(a.months + b.months),
    days: i64(a.days + b.days),
    leapdays: b.leapdays !== 0n ? b.leapdays : a.leapdays,
    hours: i64(a.hours + b.hours),
    minutes: i64(a.minutes + b.minutes),
    seconds: i64(a.seconds + b.seconds),
    microseconds: i64(a.microseconds + b.microseconds),
  };
  normalizeRelativeDelta(rel);
  return Object.freeze({
    kind: "relativedelta",
    ...rel,
    year: b.year ?? a.year,
    month: b.month ?? a.month,
    day: b.day ?? a.day,
    weekday: b.weekday ?? a.weekday,
    hour: b.hour ?? a.hour,
    minute: b.minute ?? a.minute,
    second: b.second ?? a.second,
    microsecond: b.microsecond ?? a.microsecond,
  });
}

/** Year, month and clamped day after `year`/`month`/`day` and `years`/`months` (Rust `calendar_target`). */
function calendarTarget(rd: RelativeDeltaValue, date: CalendarDate): { year: bigint; month: bigint; day: bigint } {
  let year = i64((rd.year ?? BigInt(date.year)) + rd.years);
  let month = rd.month ?? BigInt(date.month);
  if (month < 1n || month > 12n) {
    throw valueError("month must be in 1..12");
  }
  if (rd.months !== 0n) {
    month += rd.months;
    if (month > 12n) {
      year = i64(year + 1n);
      month -= 12n;
    } else if (month < 1n) {
      year = i64(year - 1n);
      month += 12n;
    }
  }
  if (year < BigInt(MIN_YEAR) || year > BigInt(MAX_YEAR)) {
    throw valueError(`year ${year} is out of range`);
  }
  const day = rd.day ?? BigInt(date.day);
  const maxDay = BigInt(daysInMonth(Number(year), Number(month)));
  return { year, month, day: day < maxDay ? day : maxDay };
}

/** The relative days plus `leapdays` when the target is after February of a leap year (checked). */
function targetDays(rd: RelativeDeltaValue, target: { year: bigint; month: bigint }): bigint {
  if (rd.leapdays !== 0n && target.month > 2n && isLeapYear(Number(target.year))) {
    return i64(rd.days + rd.leapdays);
  }
  return rd.days;
}

/** dateutil's weekday jump (Rust `apply_weekday`), with Euclidean remainders. */
function applyWeekday(date: CalendarDate, weekday: Weekday | null): CalendarDate {
  if (!weekday) return date;
  const nth = BigInt(weekday.n === null || weekday.n === 0 ? 1 : weekday.n);
  const current = weekdayOf(date.year, date.month, date.day);
  const target = weekday.day;
  let jump = i64(((nth < 0n ? -nth : nth) - 1n) * 7n);
  if (nth > 0n) {
    jump += BigInt(remEuclid(7 - current + target, 7));
  } else {
    jump += BigInt(remEuclid(current - target, 7));
    jump = -jump;
  }
  return addDays(date, jump);
}

export function relativeDeltaAddDate(d: DateValue, rd: RelativeDeltaValue): DateValue | DateTimeValue {
  if (relativeDeltaHasTime(rd)) {
    return relativeDeltaAddDateTime(dateTimeVal(d.year, d.month, d.day), rd);
  }
  const target = calendarTarget(rd, d);
  const days = targetDays(rd, target);
  const base = makeDate(target.year, target.month, target.day);
  const finalDate = applyWeekday(addDays(base, days), rd.weekday);
  return dateVal(finalDate.year, finalDate.month, finalDate.day);
}

export function relativeDeltaAddDateTime(dt: DateTimeValue, rd: RelativeDeltaValue): DateTimeValue {
  const target = calendarTarget(rd, dt);
  const days = targetDays(rd, target);
  const date = makeDate(target.year, target.month, target.day);
  const time = makeTime(
    rd.hour ?? BigInt(dt.hour),
    rd.minute ?? BigInt(dt.minute),
    rd.second ?? BigInt(dt.second),
    rd.microsecond ?? BigInt(dt.microsecond),
  );
  const baseDt = dateTimeVal(date.year, date.month, date.day, time.hour, time.minute, time.second, time.microsecond);
  // Every term is an i64 times at most 8.64e10, so the sum stays well inside i128 (as in Rust).
  const deltaMicros =
    days * MICROS_PER_DAY +
    rd.hours * 3600n * MICROS_PER_SECOND +
    rd.minutes * 60n * MICROS_PER_SECOND +
    rd.seconds * MICROS_PER_SECOND +
    rd.microseconds;
  const ret = dateTimeAddTimeDelta(baseDt, normalizeTimeDelta(deltaMicros));
  const finalDate = applyWeekday(ret, rd.weekday);
  return dateTimeVal(finalDate.year, finalDate.month, finalDate.day, ret.hour, ret.minute, ret.second, ret.microsecond);
}

// -------------------------------------------------------------------------------------------------
// Date and datetime methods (Rust `methods.rs` `date_method`)
// -------------------------------------------------------------------------------------------------

const DATE_METHODS: readonly string[] = ["strftime", "isoformat", "weekday", "isoweekday", "replace"];
const DATETIME_METHODS: readonly string[] = ["strftime", "isoformat", "weekday", "isoweekday", "replace", "date"];

/** `true` when `name` is a method of the date or datetime `recv`. */
export function isDateMethod(recv: DateValue | DateTimeValue, name: string): boolean {
  return (recv.kind === "date" ? DATE_METHODS : DATETIME_METHODS).includes(name);
}

const STRFTIME_METHOD_SIG: Sig = { name: "strftime", params: ["format"], required: 1, posOnly: 0, maxPos: 1 };
const ISOFORMAT_SIG: Sig = { name: "isoformat", params: ["sep"], required: 0, posOnly: 0, maxPos: 1 };
const DATE_REPLACE_SIG: Sig = { name: "replace", params: ["year", "month", "day"], required: 0, posOnly: 0, maxPos: 3 };
const DATETIME_REPLACE_SIG: Sig = {
  name: "replace",
  params: ["year", "month", "day", "hour", "minute", "second", "microsecond"],
  required: 0,
  posOnly: 0,
  maxPos: 7,
};

/** A string result of known length, charged before it is returned (Rust `new_str`). */
function newStr(s: string, budget: Budget): ExprValue {
  const len = utf8ByteLength(s);
  budget.alloc(len);
  budget.scan(len);
  return strVal(s);
}

/** `recv.name(*args, **kwargs)` for a date or datetime. */
export function callDateMethod(
  recv: DateValue | DateTimeValue,
  name: string,
  args: readonly ExprValue[],
  kwargs: Record<string, ExprValue>,
  budget: Budget,
): ExprValue {
  const isDt = recv.kind === "datetime";
  const time: ClockTime = isDt ? recv : MIDNIGHT;
  switch (name) {
    case "strftime": {
      const slots = bindSig(STRFTIME_METHOD_SIG, args, kwargs);
      const fmt = strArg("strftime", "1", requiredSlot(STRFTIME_METHOD_SIG, slots, 0));
      return strVal(strftime(recv, time, fmt, budget));
    }
    case "isoformat": {
      if (!isDt) {
        bindSig(sig0("isoformat"), args, kwargs);
        return newStr(dateIso(recv), budget);
      }
      const slots = bindSig(ISOFORMAT_SIG, args, kwargs);
      let sep = "T";
      const sepArg = slots[0];
      if (sepArg !== undefined) {
        const s = strArg("isoformat", "sep", sepArg);
        const scalars = [...s];
        if (scalars.length !== 1) {
          throw typeError("isoformat() argument 'sep' must be a single character");
        }
        sep = scalars[0]!;
      }
      return newStr(dateTimeIso(recv, sep), budget);
    }
    case "weekday":
      bindSig(sig0("weekday"), args, kwargs);
      return intVal(BigInt(weekdayOf(recv.year, recv.month, recv.day)));
    case "isoweekday":
      bindSig(sig0("isoweekday"), args, kwargs);
      return intVal(BigInt(weekdayOf(recv.year, recv.month, recv.day) + 1));
    case "date":
      if (isDt) {
        bindSig(sig0("date"), args, kwargs);
        return dateVal(recv.year, recv.month, recv.day);
      }
      break;
    case "replace": {
      const sig = isDt ? DATETIME_REPLACE_SIG : DATE_REPLACE_SIG;
      const slots = bindSig(sig, args, kwargs);
      const field = (i: number, current: number): bigint => {
        const v = slots[i];
        return v === undefined ? BigInt(current) : intArg("replace", sig.params[i]!, v);
      };
      const year = field(0, recv.year);
      const month = field(1, recv.month);
      const day = field(2, recv.day);
      const d = makeDate(year, month, day);
      if (!isDt) {
        return dateVal(d.year, d.month, d.day);
      }
      const hour = field(3, recv.hour);
      const minute = field(4, recv.minute);
      const second = field(5, recv.second);
      const micro = field(6, recv.microsecond);
      const t = makeTime(hour, minute, second, micro);
      return dateTimeVal(d.year, d.month, d.day, t.hour, t.minute, t.second, t.microsecond);
    }
  }
  throw attributeError(pythonTypeOf(recv), name);
}

// -------------------------------------------------------------------------------------------------
// Host Clock & Context Names
// -------------------------------------------------------------------------------------------------

export type Clock = () => DateTimeValue;

function nowFromClock(clock: Clock | undefined, what: string): DateTimeValue {
  if (!clock) {
    throw valueError(`${what} needs a clock: the host must provide one with createContext({ clock })`);
  }
  return clock();
}

const NOW_SIG: Sig = { name: "now", params: ["tz"], required: 0, posOnly: 0, maxPos: 1 };
const TIME_STRFTIME_SIG: Sig = { name: "strftime", params: ["format"], required: 1, posOnly: 1, maxPos: 1 };

export class DateTimeClass implements ExprObject {
  constructor(private readonly clock?: Clock) {}

  typeName(): string {
    return "type";
  }

  repr(): string {
    return "<class 'datetime.datetime'>";
  }

  getAttr(name: string, _budget: Budget): ExprValue {
    if (name === "now" || name === "today") {
      return callableVal(name, (args, kwargs, _callBudget) => {
        if (name === "now") {
          const slots = bindSig(NOW_SIG, args, kwargs);
          const tz = slots[0];
          if (tz !== undefined && tz.kind !== "none") {
            throw valueError("timezone-aware datetimes are not supported in expressions (naive UTC only)");
          }
        } else {
          bindSig(sig0("today"), args, kwargs);
        }
        return nowFromClock(this.clock, "datetime.now()");
      });
    }
    throw typeAttributeError("datetime.datetime", name);
  }

  call(args: readonly ExprValue[], kwargs: Record<string, ExprValue>, _budget: Budget): ExprValue {
    return createDateTime(args, kwargs);
  }
}

export class TimeModule implements ExprObject {
  constructor(private readonly clock?: Clock) {}

  typeName(): string {
    return "module";
  }

  repr(): string {
    return "<module 'time' (built-in)>";
  }

  getAttr(name: string, _budget: Budget): ExprValue {
    if (name === "time") {
      return callableVal("time", (args, kwargs, _callBudget) => {
        bindSig(sig0("time"), args, kwargs);
        const dt = nowFromClock(this.clock, "time.time()");
        const days = toDays(dt.year, dt.month, dt.day);
        // Unix epoch 1970-01-01 is day 719162
        const unixDays = days - 719162;
        const secs =
          unixDays * 86400 +
          dt.hour * 3600 +
          dt.minute * 60 +
          dt.second +
          dt.microsecond / 1e6;
        return floatVal(secs);
      });
    }
    if (name === "strftime") {
      return callableVal("strftime", (args, kwargs, callBudget) => {
        const slots = bindSig(TIME_STRFTIME_SIG, args, kwargs);
        const fmt = strArg("strftime", "format", requiredSlot(TIME_STRFTIME_SIG, slots, 0));
        const dt = nowFromClock(this.clock, "time.strftime()");
        return strVal(strftime(dt, dt, fmt, callBudget));
      });
    }
    throw moduleAttributeError("time", name);
  }
}

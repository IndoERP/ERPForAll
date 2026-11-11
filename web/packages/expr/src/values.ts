/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Budget, SLOT } from "./budget";
import { typeError } from "./python-error";

export interface NoneValue {
  readonly kind: "none";
}

export interface BoolValue {
  readonly kind: "bool";
  readonly value: boolean;
}

export interface IntValue {
  readonly kind: "int";
  readonly value: bigint;
}

export interface FloatValue {
  readonly kind: "float";
  readonly value: number;
}

export interface StrValue {
  readonly kind: "str";
  readonly value: string;
}

export interface BytesValue {
  readonly kind: "bytes";
  readonly value: Uint8Array;
}

export interface ListValue {
  readonly kind: "list";
  readonly items: readonly ExprValue[];
}

export interface TupleValue {
  readonly kind: "tuple";
  readonly items: readonly ExprValue[];
}

export interface DictValue {
  readonly kind: "dict";
  readonly entries: readonly (readonly [ExprValue, ExprValue])[];
}

export interface ExprObject {
  typeName(): string;
  isTruthy?(): boolean;
  getAttr(name: string, budget: Budget): ExprValue;
  call?(args: readonly ExprValue[], kwargs: Record<string, ExprValue>, budget: Budget): ExprValue;
  getItem?(key: ExprValue, budget: Budget): ExprValue;
  repr?(): string;
}

export interface ObjectValue {
  readonly kind: "object";
  readonly handle: ExprObject;
}

export type NativeCallable = (
  args: readonly ExprValue[],
  kwargs: Record<string, ExprValue>,
  budget: Budget,
) => ExprValue;

/**
 * A keyword argument by own key only. Keyword containers are created without a prototype
 * (`newKwargs`), and this lookup also refuses inherited JS properties (`constructor`, `toString`)
 * when a host passes an ordinary object.
 */
export function ownKwarg(kwargs: Record<string, ExprValue>, name: string): ExprValue | undefined {
  return Object.hasOwn(kwargs, name) ? kwargs[name] : undefined;
}

/** An empty keyword container with no prototype, so `__proto__` is an ordinary key. */
export function newKwargs(): Record<string, ExprValue> {
  return Object.create(null) as Record<string, ExprValue>;
}

export interface CallableValue {
  readonly kind: "callable";
  readonly name: string;
  /** The name CPython prints in argument errors (`str.format`, `dict.get`); defaults to `name`. */
  readonly qualname?: string;
  readonly call: NativeCallable;
}

export interface DateValue {
  readonly kind: "date";
  readonly year: number;
  readonly month: number;
  readonly day: number;
}

export interface DateTimeValue {
  readonly kind: "datetime";
  readonly year: number;
  readonly month: number;
  readonly day: number;
  readonly hour: number;
  readonly minute: number;
  readonly second: number;
  readonly microsecond: number;
}

export interface TimeDeltaValue {
  readonly kind: "timedelta";
  readonly days: number;
  readonly seconds: number;
  readonly microseconds: number;
}

export interface Weekday {
  readonly day: number;
  readonly n: number | null;
}

/** `dateutil.relativedelta`; every field is an i64 as in `crates/erp-expr` (exact, checked). */
export interface RelativeDeltaValue {
  readonly kind: "relativedelta";
  readonly years: bigint;
  readonly months: bigint;
  readonly days: bigint;
  readonly leapdays: bigint;
  readonly hours: bigint;
  readonly minutes: bigint;
  readonly seconds: bigint;
  readonly microseconds: bigint;
  readonly year: bigint | null;
  readonly month: bigint | null;
  readonly day: bigint | null;
  readonly weekday: Weekday | null;
  readonly hour: bigint | null;
  readonly minute: bigint | null;
  readonly second: bigint | null;
  readonly microsecond: bigint | null;
}

export type ExprValue =
  | NoneValue
  | BoolValue
  | IntValue
  | FloatValue
  | StrValue
  | BytesValue
  | ListValue
  | TupleValue
  | DictValue
  | ObjectValue
  | CallableValue
  | DateValue
  | DateTimeValue
  | TimeDeltaValue
  | RelativeDeltaValue;

export const NONE: NoneValue = Object.freeze({ kind: "none" });
export const TRUE: BoolValue = Object.freeze({ kind: "bool", value: true });
export const FALSE: BoolValue = Object.freeze({ kind: "bool", value: false });

export function boolVal(value: boolean): BoolValue {
  return value ? TRUE : FALSE;
}

export function intVal(value: bigint | number | string): IntValue {
  const b = typeof value === "bigint" ? value : BigInt(value);
  return Object.freeze({ kind: "int", value: b });
}

export function floatVal(value: number): FloatValue {
  return Object.freeze({ kind: "float", value });
}

export function strVal(value: string): StrValue {
  return Object.freeze({ kind: "str", value });
}

export function bytesVal(value: Uint8Array | readonly number[]): BytesValue {
  if (value instanceof Uint8Array) {
    new Budget().checkValue({ kind: "bytes", value });
    return Object.freeze({ kind: "bytes", value });
  }
  if (!Array.isArray(value)) throw typeError("invalid expression value");
  new Budget().checkSize(SLOT + value.length);
  for (let i = 0; i < value.length; i++) {
    const descriptor = Object.getOwnPropertyDescriptor(value, i);
    const byte: unknown = descriptor && "value" in descriptor ? descriptor.value : undefined;
    if (typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > 255) throw typeError("invalid expression value");
  }
  const bytes = new Uint8Array(value.length);
  for (let i = 0; i < value.length; i++) bytes[i] = value[i]!;
  return Object.freeze({ kind: "bytes", value: bytes });
}

/** Bound trusted container inputs before copying; external admission applies the current lower budget. */
function copySequence(kind: "list" | "tuple", items: readonly ExprValue[]): readonly ExprValue[] {
  new Budget().checkValue({ kind, items });
  const copy: ExprValue[] = [];
  // Index traversal cannot invoke a custom data iterator. Validation already refused data getters.
  for (let i = 0; i < items.length; i++) copy.push(items[i]!);
  return Object.freeze(copy);
}

export function listVal(items: readonly ExprValue[]): ListValue {
  return Object.freeze({ kind: "list", items: copySequence("list", items) });
}

export function tupleVal(items: readonly ExprValue[]): TupleValue {
  return Object.freeze({ kind: "tuple", items: copySequence("tuple", items) });
}

export function dictVal(entries: readonly (readonly [ExprValue, ExprValue])[]): DictValue {
  new Budget().checkValue({ kind: "dict", entries });
  const copy: (readonly [ExprValue, ExprValue])[] = [];
  for (let i = 0; i < entries.length; i++) {
    const pair = entries[i]!;
    copy.push(Object.freeze([pair[0], pair[1]] as const));
  }
  return Object.freeze({ kind: "dict", entries: Object.freeze(copy) });
}

const registeredCapabilities = new WeakSet<object>();
const capturedValues = new WeakMap<object, readonly ExprValue[]>();
/** Internal immutable receiver metadata for bound native methods (not a package export). */
export function withCapturedValue<T extends ExprValue>(value: T, receiver: ExprValue): T {
  capturedValues.set(value, Object.freeze([receiver]));
  return value;
}
export function capabilityCaptures(value: object): readonly ExprValue[] { return capturedValues.get(value) ?? []; }

/** Internal membership check; a forged tagged object is never a native capability. */
export function isRegisteredCapability(value: object): boolean { return registeredCapabilities.has(value); }

export function objectVal(handle: ExprObject): ObjectValue {
  const value = Object.freeze({ kind: "object" as const, handle });
  registeredCapabilities.add(value);
  return value;
}

export function callableVal(name: string, call: NativeCallable, qualname?: string): CallableValue {
  const value: CallableValue = Object.freeze(qualname === undefined ? { kind: "callable", name, call } : { kind: "callable", name, qualname, call });
  registeredCapabilities.add(value);
  return value;
}

export function dateVal(year: number, month: number, day: number): DateValue {
  return Object.freeze({ kind: "date", year, month, day });
}

export function dateTimeVal(
  year: number,
  month: number,
  day: number,
  hour: number = 0,
  minute: number = 0,
  second: number = 0,
  microsecond: number = 0,
): DateTimeValue {
  return Object.freeze({ kind: "datetime", year, month, day, hour, minute, second, microsecond });
}

export function timeDeltaVal(days: number = 0, seconds: number = 0, microseconds: number = 0): TimeDeltaValue {
  return Object.freeze({ kind: "timedelta", days, seconds, microseconds });
}

export function relativeDeltaVal(fields: {
  years?: bigint;
  months?: bigint;
  days?: bigint;
  leapdays?: bigint;
  hours?: bigint;
  minutes?: bigint;
  seconds?: bigint;
  microseconds?: bigint;
  year?: bigint | null;
  month?: bigint | null;
  day?: bigint | null;
  weekday?: Weekday | null;
  hour?: bigint | null;
  minute?: bigint | null;
  second?: bigint | null;
  microsecond?: bigint | null;
}): RelativeDeltaValue {
  return Object.freeze({
    kind: "relativedelta",
    years: fields.years ?? 0n,
    months: fields.months ?? 0n,
    days: fields.days ?? 0n,
    leapdays: fields.leapdays ?? 0n,
    hours: fields.hours ?? 0n,
    minutes: fields.minutes ?? 0n,
    seconds: fields.seconds ?? 0n,
    microseconds: fields.microseconds ?? 0n,
    year: fields.year ?? null,
    month: fields.month ?? null,
    day: fields.day ?? null,
    weekday: fields.weekday ?? null,
    hour: fields.hour ?? null,
    minute: fields.minute ?? null,
    second: fields.second ?? null,
    microsecond: fields.microsecond ?? null,
  });
}

export function isTruthy(val: ExprValue): boolean {
  switch (val.kind) {
    case "none":
      return false;
    case "bool":
      return val.value;
    case "int":
      return val.value !== 0n;
    case "float":
      return val.value !== 0 && !Number.isNaN(val.value);
    case "str":
      return val.value.length > 0;
    case "bytes":
      return val.value.length > 0;
    case "list":
    case "tuple":
      return val.items.length > 0;
    case "dict":
      return val.entries.length > 0;
    case "object":
      return val.handle.isTruthy ? val.handle.isTruthy() : true;
    case "callable":
      return true;
    case "date":
    case "datetime":
      return true;
    case "timedelta":
      return val.days !== 0 || val.seconds !== 0 || val.microseconds !== 0;
    case "relativedelta":
      return (
        val.years !== 0n ||
        val.months !== 0n ||
        val.days !== 0n ||
        val.leapdays !== 0n ||
        val.hours !== 0n ||
        val.minutes !== 0n ||
        val.seconds !== 0n ||
        val.microseconds !== 0n ||
        val.year !== null ||
        val.month !== null ||
        val.day !== null ||
        val.weekday !== null ||
        val.hour !== null ||
        val.minute !== null ||
        val.second !== null ||
        val.microsecond !== null
      );
  }
}

export function pythonTypeOf(val: ExprValue): string {
  switch (val.kind) {
    case "none":
      return "NoneType";
    case "bool":
      return "bool";
    case "int":
      return "int";
    case "float":
      return "float";
    case "str":
      return "str";
    case "bytes":
      return "bytes";
    case "list":
      return "list";
    case "tuple":
      return "tuple";
    case "dict":
      return "dict";
    case "object":
      return val.handle.typeName();
    case "callable":
      return "builtin_function_or_method";
    case "date":
      return "datetime.date";
    case "datetime":
      return "datetime.datetime";
    case "timedelta":
      return "datetime.timedelta";
    case "relativedelta":
      return "relativedelta";
  }
}

function escapePythonString(s: string): string {
  let res = "";
  let hasSingleQuote = false;
  let hasDoubleQuote = false;
  for (const ch of s) {
    if (ch === "'") hasSingleQuote = true;
    if (ch === '"') hasDoubleQuote = true;
  }
  const quote = hasSingleQuote && !hasDoubleQuote ? '"' : "'";
  for (let i = 0; i < s.length; i++) {
    const ch = s[i]!;
    if (ch === "\\") res += "\\\\";
    else if (ch === quote) res += `\\${quote}`;
    else if (ch === "\n") res += "\\n";
    else if (ch === "\r") res += "\\r";
    else if (ch === "\t") res += "\\t";
    else if (ch < " " || ch === "\x7f") {
      const hex = ch.charCodeAt(0).toString(16).padStart(2, "0");
      res += `\\x${hex}`;
    } else {
      res += ch;
    }
  }
  return `${quote}${res}${quote}`;
}

export function floatRepr(x: number): string {
  if (Number.isNaN(x)) return "nan";
  if (x === Infinity) return "inf";
  if (x === -Infinity) return "-inf";
  if (Object.is(x, -0)) return "-0.0";

  const sci = x.toExponential();
  const [mantissa, expStr] = sci.split("e");
  if (!mantissa || expStr === undefined) return x.toString();
  const exp = parseInt(expStr, 10);
  const negative = mantissa.startsWith("-");
  const digits = mantissa.replace(/[^0-9]/g, "");
  let out = negative ? "-" : "";

  if (exp >= -4 && exp < 16) {
    if (exp >= 0) {
      const intLen = exp + 1;
      if (digits.length <= intLen) {
        out += digits + "0".repeat(intLen - digits.length) + ".0";
      } else {
        out += digits.slice(0, intLen) + "." + digits.slice(intLen);
      }
    } else {
      out += "0." + "0".repeat(-exp - 1) + digits;
    }
  } else {
    out += digits[0];
    if (digits.length > 1) {
      out += "." + digits.slice(1);
    }
    out += "e" + (exp < 0 ? "-" : "+") + (exp < 0 ? String(-exp).padStart(2, "0") : String(exp).padStart(2, "0"));
  }
  return out;
}

export function repr(val: ExprValue): string {
  switch (val.kind) {
    case "none":
      return "None";
    case "bool":
      return val.value ? "True" : "False";
    case "int":
      return val.value.toString();
    case "float":
      return floatRepr(val.value);
    case "str":
      return escapePythonString(val.value);
    case "bytes": {
      let b = "b'";
      for (const byte of val.value) {
        if (byte === 0x5c) b += "\\\\";
        else if (byte === 0x27) b += "\\'";
        else if (byte === 0x0a) b += "\\n";
        else if (byte === 0x0d) b += "\\r";
        else if (byte === 0x09) b += "\\t";
        else if (byte >= 0x20 && byte <= 0x7e) b += String.fromCharCode(byte);
        else {
          b += `\\x${byte.toString(16).padStart(2, "0")}`;
        }
      }
      b += "'";
      return b;
    }
    case "list":
      return `[${val.items.map(repr).join(", ")}]`;
    case "tuple":
      if (val.items.length === 1) {
        return `(${repr(val.items[0]!)},)`;
      }
      return `(${val.items.map(repr).join(", ")})`;
    case "dict": {
      const parts = val.entries.map(([k, v]) => `${repr(k)}: ${repr(v)}`);
      return `{${parts.join(", ")}}`;
    }
    case "object":
      return val.handle.repr ? val.handle.repr() : `<${val.handle.typeName()} object>`;
    case "callable":
      return `<built-in function ${val.name}>`;
    case "date":
      return `datetime.date(${val.year}, ${val.month}, ${val.day})`;
    case "datetime": {
      let s = `datetime.datetime(${val.year}, ${val.month}, ${val.day}, ${val.hour}, ${val.minute}`;
      if (val.second !== 0 || val.microsecond !== 0) {
        s += `, ${val.second}`;
      }
      if (val.microsecond !== 0) {
        s += `, ${val.microsecond}`;
      }
      s += ")";
      return s;
    }
    case "timedelta": {
      const parts: string[] = [];
      if (val.days !== 0) parts.push(`days=${val.days}`);
      if (val.seconds !== 0) parts.push(`seconds=${val.seconds}`);
      if (val.microseconds !== 0) parts.push(`microseconds=${val.microseconds}`);
      if (parts.length === 0) return "datetime.timedelta(0)";
      return `datetime.timedelta(${parts.join(", ")})`;
    }
    case "relativedelta": {
      const parts: string[] = [];
      const relFields: [string, bigint][] = [
        ["years", val.years],
        ["months", val.months],
        ["days", val.days],
        ["leapdays", val.leapdays],
        ["hours", val.hours],
        ["minutes", val.minutes],
        ["seconds", val.seconds],
        ["microseconds", val.microseconds],
      ];
      for (const [name, v] of relFields) {
        if (v !== 0n) {
          const signStr = v > 0n ? `+${v}` : `${v}`;
          parts.push(`${name}=${signStr}`);
        }
      }
      const absFields1: [string, bigint | null][] = [
        ["year", val.year],
        ["month", val.month],
        ["day", val.day],
      ];
      for (const [name, v] of absFields1) {
        if (v !== null) parts.push(`${name}=${v}`);
      }
      if (val.weekday !== null) {
        const WEEKDAY_NAMES = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"];
        const name = WEEKDAY_NAMES[val.weekday.day] ?? "??";
        if (val.weekday.n !== null && val.weekday.n !== 0) {
          const signStr = val.weekday.n > 0 ? `+${val.weekday.n}` : `${val.weekday.n}`;
          parts.push(`weekday=${name}(${signStr})`);
        } else {
          parts.push(`weekday=${name}`);
        }
      }
      const absFields2: [string, bigint | null][] = [
        ["hour", val.hour],
        ["minute", val.minute],
        ["second", val.second],
        ["microsecond", val.microsecond],
      ];
      for (const [name, v] of absFields2) {
        if (v !== null) parts.push(`${name}=${v}`);
      }
      return `relativedelta(${parts.join(", ")})`;
    }
  }
}

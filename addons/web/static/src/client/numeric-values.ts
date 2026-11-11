/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Locale } from "../generated/human-ui.js";
import { ClientError } from "./transport.js";

/** Explicit numeric settings from res.lang; unrelated bootstrap members are not consumed here. */
export type NumericLocale = Pick<Locale, "decimal_point" | "thousands_sep" | "grouping">;
export type NumericKind = "integer" | "float";

// Admission bounds, not performance measurements. One UTF-16 input unit needs at most three UTF-8 bytes.
const INPUT_LIMIT = 256;
const GROUPING_LIMIT = 8;
const DISPLAY_DIGITS_LIMIT = 20;

function checkedKind(kind: NumericKind): void {
  if (kind !== "integer" && kind !== "float") throw new ClientError("validation", "Invalid numeric kind.");
}

function checkedLocale(locale: NumericLocale): void {
  const fail = (): never => { throw new ClientError("server", "Invalid numeric locale."); };
  if (locale === null || typeof locale !== "object" || Array.isArray(locale)) fail();
  const keys = Reflect.ownKeys(locale);
  if (keys.length !== 3 || keys.some(key => key !== "decimal_point" && key !== "thousands_sep" && key !== "grouping")) fail();
  const decimal = locale.decimal_point, separator = locale.thousands_sep;
  if (typeof decimal !== "string" || [...decimal].length !== 1 || /[\s0-9+\-eE\u0000-\u001f\u007f]/u.test(decimal)) fail();
  if (typeof separator !== "string" || separator !== "" && ([...separator].length !== 1 || /[0-9+\-eE\u0000-\u001f\u007f]/u.test(separator)) || decimal === separator) fail();
  const grouping = locale.grouping;
  if (!Array.isArray(grouping) || grouping.length > GROUPING_LIMIT) fail();
  for (const [index, width] of grouping.entries()) {
    if (!Number.isInteger(width) || width < 0 || width > 9 && width !== 127) fail();
    if ((width === 0 || width === 127) && index !== grouping.length - 1 || width === 0 && index === 0) fail();
  }
}

/** Applies a finite grouping program: 0 repeats the previous width, 127 stops grouping. */
function groupInteger(digits: string, locale: NumericLocale): string {
  if (locale.thousands_sep === "" || !locale.grouping.length || locale.grouping[0] === 127) return digits;
  const groups: string[] = [];
  let end = digits.length, index = 0, previous = 0;
  while (end > 0) {
    const instruction = locale.grouping[index];
    if (instruction === undefined || instruction === 127) break;
    const width = instruction === 0 ? previous : instruction;
    if (end <= width) break;
    groups.push(digits.slice(end - width, end));
    end -= width;
    previous = width;
    if (instruction !== 0) index++;
  }
  groups.push(digits.slice(0, end));
  return groups.reverse().join(locale.thousands_sep);
}

/** Number.toString may use an exponent; expand its bounded, finite representation without rounding. */
function decimalText(value: number): string {
  const text = String(value);
  const exponentAt = text.indexOf("e");
  if (exponentAt === -1) return text;
  const mantissa = text.slice(0, exponentAt), exponent = Number(text.slice(exponentAt + 1));
  const dot = mantissa.indexOf(".");
  const before = dot === -1 ? mantissa.length : dot;
  const digits = mantissa.replace(".", "");
  const position = before + exponent;
  if (position <= 0) return `0.${"0".repeat(-position)}${digits}`;
  if (position >= digits.length) return digits + "0".repeat(position - digits.length);
  return `${digits.slice(0, position)}.${digits.slice(position)}`;
}

function canonicalDecimal(whole: string, fraction: string): string {
  const integer = whole.replace(/^0+/, "") || "0";
  const decimal = fraction.replace(/0+$/, "");
  return decimal ? `${integer}.${decimal}` : integer;
}

function invalidInput(input: string, kind: NumericKind): never {
  const message = kind === "integer"
    ? `Enter a valid integer between -9007199254740991 and 9007199254740991: ${JSON.stringify(input)}.`
    : `Enter a valid finite number: ${JSON.stringify(input)}.`;
  throw new ClientError("validation", message);
}

/**
 * Parse a localized decimal draft into the existing JSON numeric value contract.
 * Empty input is false; exponent/source expressions and lossy decimal conversions are refused.
 * The caller retains the draft on failure and decides when a successful result enters RecordState.
 */
export function parseNumeric(input: string, kind: NumericKind, locale: NumericLocale): number | false {
  checkedKind(kind); checkedLocale(locale);
  if (typeof input !== "string") throw new ClientError("validation", "Numeric input must be text.");
  if (input.length > INPUT_LIMIT) throw new ClientError("validation", "Numeric input exceeds 256 characters.");
  if (input.trim() === "") return false;
  // A declared space grouping separator is data, including at either edge. Other ASCII whitespace
  // may surround a draft; interior whitespace remains invalid instead of being stripped.
  const padding = locale.thousands_sep === " " ? /^[\t\r\n]+|[\t\r\n]+$/g : /^[ \t\r\n]+|[ \t\r\n]+$/g;
  let draft = input.replace(padding, "");
  const negative = draft.startsWith("-");
  if (negative || draft.startsWith("+")) draft = draft.slice(1);
  const parts = draft.split(locale.decimal_point);
  if (parts.length > 2 || kind === "integer" && parts.length !== 1) invalidInput(input, kind);
  const localizedWhole = parts[0];
  // split() always contains its first member, including for an empty string.
  if (localizedWhole === undefined) invalidInput(input, kind);
  const fraction = parts.length === 2 ? parts[1] : "";
  if (fraction === undefined || !/^[0-9]*$/.test(fraction)) invalidInput(input, kind);
  const separator = locale.thousands_sep;
  const whole = separator === "" ? localizedWhole : localizedWhole.split(separator).join("");
  if (!/^[0-9]*$/.test(whole) || whole === "" && fraction === "") invalidInput(input, kind);
  if (separator !== "" && localizedWhole.includes(separator) && groupInteger(whole, locale) !== localizedWhole) invalidInput(input, kind);
  const canonical = canonicalDecimal(whole, fraction);
  const value = Number(`${negative ? "-" : ""}${canonical}`);
  if (!Number.isFinite(value) || Math.abs(value) > Number.MAX_SAFE_INTEGER || kind === "integer" && !Number.isSafeInteger(value)) invalidInput(input, kind);
  const represented = decimalText(Math.abs(value)).split(".");
  const representedWhole = represented[0];
  if (representedWhole === undefined) throw new ClientError("server", "Invalid represented numeric value.");
  if (canonical !== canonicalDecimal(representedWhole, represented[1] ?? "")) {
    throw new ClientError("validation", `Numeric input loses precision: ${JSON.stringify(input)}.`);
  }
  return value;
}

/**
 * Format a represented JSON number using explicit res.lang settings.
 * digits affects this returned presentation only; this function never changes the represented value.
 */
export function formatNumeric(value: number | false, kind: NumericKind, locale: NumericLocale, digits?: number): string {
  checkedKind(kind); checkedLocale(locale);
  if (digits !== undefined && (!Number.isInteger(digits) || digits < 0 || digits > DISPLAY_DIGITS_LIMIT)) {
    throw new ClientError("validation", "Numeric display precision must be an integer from 0 to 20.");
  }
  if (kind === "integer" && digits !== undefined && digits !== 0) {
    throw new ClientError("validation", "Integer fields cannot declare fractional display precision.");
  }
  if (value === false) return "";
  if (typeof value !== "number" || !Number.isFinite(value) || Math.abs(value) > Number.MAX_SAFE_INTEGER || kind === "integer" && !Number.isSafeInteger(value)) {
    throw new ClientError("server", "Invalid represented numeric value.");
  }
  const text = digits === undefined ? decimalText(Math.abs(value)) : Math.abs(value).toFixed(digits);
  const parts = text.split(".");
  const whole = parts[0];
  if (whole === undefined) throw new ClientError("server", "Invalid represented numeric value.");
  const result = `${value < 0 ? "-" : ""}${groupInteger(whole, locale)}${parts[1] === undefined ? "" : locale.decimal_point + parts[1]}`;
  // Finite IEEE-754 expansion is bounded; keep this explicit if representation changes in future.
  if (result.length > 384) throw new ClientError("server", "Numeric display exceeds 384 characters.");
  return result;
}

/** Move the decimal point of a finite number by `places` digits through its decimal text, so no binary rounding enters. */
function shiftDecimal(value: number, places: number): number {
  if (typeof value !== "number" || !Number.isFinite(value)) throw new ClientError("server", "Invalid represented numeric value.");
  const [whole = "0", fraction = ""] = decimalText(Math.abs(value)).split(".");
  const digits = whole + fraction, point = whole.length + places;
  const text = point <= 0 ? `0.${"0".repeat(-point)}${digits}` : point >= digits.length ? digits + "0".repeat(point - digits.length) : `${digits.slice(0, point)}.${digits.slice(point)}`;
  const shifted = Number(`${value < 0 ? "-" : ""}${text}`);
  if (!Number.isFinite(shifted) || Math.abs(shifted) > Number.MAX_SAFE_INTEGER) throw new ClientError("validation", "Percentage is outside the supported range.");
  return shifted;
}

/** Stored fraction to the percent a user reads (0.07 becomes 7), exact in decimal. */
export function percentFromFraction(fraction: number): number { return shiftDecimal(fraction, 2); }

/** Percent a user typed to the stored fraction (7 becomes 0.07), exact in decimal. */
export function fractionFromPercent(percent: number): number { return shiftDecimal(percent, -2); }

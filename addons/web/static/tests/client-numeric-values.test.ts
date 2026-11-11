/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { formatNumeric, parseNumeric, type NumericLocale } from "../src/client/numeric-values.js";

const english: NumericLocale = { decimal_point: ".", thousands_sep: ",", grouping: [3, 0] };
const european: NumericLocale = { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] };
const french: NumericLocale = { decimal_point: ",", thousands_sep: "\u202f", grouping: [3, 0] };
const indian: NumericLocale = { decimal_point: ".", thousands_sep: ",", grouping: [3, 2, 0] };
const invalid = (action: () => unknown, kind: string, message: string): void => {
  expect(action).toThrowError(expect.objectContaining({ name: "ClientError", kind, message }));
};

/// (partial: bounded localized numeric conversion; no field adapter or served-view proof)
test("numeric codec distinguishes empty values from zero without ambient locale", () => {
  expect(parseNumeric("", "integer", english)).toBe(false);
  expect(parseNumeric("  ", "float", english)).toBe(false);
  expect(parseNumeric("0", "integer", english)).toBe(0);
  expect(parseNumeric("0,00", "float", european)).toBe(0);
  expect(parseNumeric("-0", "float", english)).toBe(-0);
  expect(formatNumeric(false, "float", english)).toBe("");
  expect(formatNumeric(0, "integer", english)).toBe("0");
  expect(formatNumeric(0, "float", european, 2)).toBe("0,00");
});

test("numeric codec parses explicit locale separators including non-ASCII grouping", () => {
  expect(parseNumeric("1,234,567.89", "float", english)).toBe(1234567.89);
  expect(parseNumeric("-1.234.567,89", "float", european)).toBe(-1234567.89);
  expect(parseNumeric("1\u202f234\u202f567,89", "float", french)).toBe(1234567.89);
  expect(parseNumeric("12,34,567.89", "float", indian)).toBe(1234567.89);
  expect(parseNumeric("1234567.89", "float", indian)).toBe(1234567.89);
  expect(parseNumeric("+12.50", "float", english)).toBe(12.5);
  expect(parseNumeric("  -1234.50  ", "float", english)).toBe(-1234.5);
});

test("numeric codec validates every supplied grouping segment instead of stripping separators", () => {
  for (const input of ["12,34", "1,,234", ",123", "123,", "1234,567", "1,234.5,6", "1 234.5", "1,234,56"]) {
    invalid(() => parseNumeric(input, "float", english), "validation", `Enter a valid finite number: ${JSON.stringify(input)}.`);
  }
  invalid(() => parseNumeric("1,234,567", "integer", indian), "validation", "Enter a valid integer between -9007199254740991 and 9007199254740991: \"1,234,567\".");
});

test("numeric codec supports explicit grouping stop and no grouping without guessing", () => {
  const stop: NumericLocale = { ...english, grouping: [3, 127] };
  expect(parseNumeric("1234,567", "integer", stop)).toBe(1234567);
  expect(formatNumeric(1234567, "integer", stop)).toBe("1234,567");
  expect(formatNumeric(1234567, "integer", { ...english, grouping: [3] })).toBe("1234,567");
  expect(formatNumeric(1234567, "integer", { ...english, grouping: [] })).toBe("1234567");
  expect(formatNumeric(1234567, "integer", { ...english, grouping: [127] })).toBe("1234567");
  expect(parseNumeric("1234567", "integer", { ...english, grouping: [] })).toBe(1234567);
  invalid(() => parseNumeric("1,234", "float", { ...english, grouping: [] }), "validation", "Enter a valid finite number: \"1,234\".");
  expect(formatNumeric(1234.5, "float", { ...english, thousands_sep: "" })).toBe("1234.5");
});

test("numeric codec formats grouping and decimal precision as presentation only", () => {
  const represented = 1234567.125;
  expect(formatNumeric(represented, "float", european)).toBe("1.234.567,125");
  expect(formatNumeric(represented, "float", french, 2)).toBe("1\u202f234\u202f567,13");
  expect(formatNumeric(represented, "float", indian, 4)).toBe("12,34,567.1250");
  expect(represented).toBe(1234567.125);
  expect(parseNumeric(formatNumeric(represented, "float", european), "float", european)).toBe(represented);
  expect(formatNumeric(-1234.5, "float", english, 0)).toBe("-1,235");
  expect(formatNumeric(0.0000001, "float", english)).toBe("0.0000001");
  expect(parseNumeric("0.0000001", "float", english)).toBe(0.0000001);
});

test("numeric codec rejects malformed, nonfinite, expression and exponent input", () => {
  for (const input of ["NaN", "Infinity", "-Infinity", "0x10", "1e3", "1e-3", "1+2", "1_000", "1.2.3", ".", "-", "--1", "12abc", "١٢", "1\n2", "<script>"]) {
    invalid(() => parseNumeric(input, "float", english), "validation", `Enter a valid finite number: ${JSON.stringify(input)}.`);
  }
  expect(parseNumeric(".5", "float", english)).toBe(0.5);
  expect(parseNumeric("1.", "float", english)).toBe(1);
});

test("numeric codec keeps integer bounds explicit and rejects fractional integer input", () => {
  expect(parseNumeric("9,007,199,254,740,991", "integer", english)).toBe(Number.MAX_SAFE_INTEGER);
  expect(parseNumeric("-9,007,199,254,740,991", "integer", english)).toBe(Number.MIN_SAFE_INTEGER);
  for (const input of ["9007199254740992", "-9007199254740992", "1.5", "1.0", "1e3"]) {
    invalid(() => parseNumeric(input, "integer", english), "validation", `Enter a valid integer between -9007199254740991 and 9007199254740991: ${JSON.stringify(input)}.`);
  }
});

test("numeric codec refuses decimal input that Number conversion would silently change", () => {
  for (const input of ["0.10000000000000001", "1.0000000000000001", "9007199254740990.5", "123456789012345.6789"]) {
    invalid(() => parseNumeric(input, "float", english), "validation", `Numeric input loses precision: ${JSON.stringify(input)}.`);
  }
  expect(parseNumeric("00001.2500", "float", english)).toBe(1.25);
  expect(parseNumeric("0.1", "float", english)).toBe(0.1);
  invalid(() => parseNumeric("9007199254740992", "float", english), "validation", "Enter a valid finite number: \"9007199254740992\".");
});

test("numeric codec bounds input and rejects wrong types without substitution", () => {
  invalid(() => parseNumeric("1".repeat(257), "float", english), "validation", "Numeric input exceeds 256 characters.");
  invalid(() => parseNumeric(12 as unknown as string, "float", english), "validation", "Numeric input must be text.");
  invalid(() => parseNumeric("12", "money" as "float", english), "validation", "Invalid numeric kind.");
  invalid(() => formatNumeric(12, "money" as "float", english), "validation", "Invalid numeric kind.");
});

test("numeric codec validates locale shape, separators and complete grouping program", () => {
  const locales: unknown[] = [null, [], {}, { ...english, lang: "en_GB" }, { ...english, decimal_point: "" }, { ...english, decimal_point: "," },
    { ...english, decimal_point: "1" }, { ...english, decimal_point: " " }, { ...english, thousands_sep: "-" }, { ...english, thousands_sep: "\n" },
    { ...english, grouping: [0] }, { ...english, grouping: [3, 0, 2] }, { ...english, grouping: [3, 127, 2] },
    { ...english, grouping: [3.5] }, { ...english, grouping: [10] }, { ...english, grouping: "[3,0]" }, { ...english, grouping: Array(9).fill(3) }];
  for (const raw of locales) {
    invalid(() => parseNumeric("1", "float", raw as NumericLocale), "server", "Invalid numeric locale.");
    invalid(() => formatNumeric(1, "float", raw as NumericLocale), "server", "Invalid numeric locale.");
  }
});

test("numeric codec rejects invalid represented values and precision options", () => {
  for (const value of [NaN, Infinity, -Infinity, 9007199254740992, "12", null]) {
    invalid(() => formatNumeric(value as number, "float", english), "server", "Invalid represented numeric value.");
  }
  invalid(() => formatNumeric(1.2, "integer", english), "server", "Invalid represented numeric value.");
  for (const digits of [-1, 21, 1.5, NaN, "2"]) {
    invalid(() => formatNumeric(1, "float", english, digits as number), "validation", "Numeric display precision must be an integer from 0 to 20.");
  }
  invalid(() => formatNumeric(1, "integer", english, 2), "validation", "Integer fields cannot declare fractional display precision.");
});

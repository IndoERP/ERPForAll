/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import {
  editorLocale, formatEditorValue, formatTimeValue, parseEditorValue, parseTimeValue,
  partsFromValue, roundTimeValue, timeSuggestions, valueFromParts,
  type EditorLocale, type ParseResult,
} from "../src/client/datetime-values.js";

const locale: EditorLocale = { lang: "en_GB", tz: "UTC", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" };
const inZone = (tz: string): EditorLocale => ({ ...locale, tz });
function invalid(result: ParseResult, message: string): void {
  expect(result).toEqual({ ok: false, error: expect.objectContaining({ kind: "validation", message }) });
}

test("locale validates required members without replacing timezone or formatting configuration", () => {
  expect(editorLocale(locale)).toEqual(locale);
  for (const key of ["lang", "tz", "date_format", "time_format"] as const) {
    const raw: Record<string, unknown> = { ...locale }; delete raw[key];
    expect(() => editorLocale(raw)).toThrowError(expect.objectContaining({ kind: "server", message: `Invalid editor locale ${key}.` }));
  }
  expect(() => editorLocale({ ...locale, tz: "Not/A_Zone" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unsupported editor timezone: Not/A_Zone." }));
  expect(() => editorLocale({ ...locale, date_format: "%Q" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %Q." }));
});

// Date conversion subset; the parameterized cases are not parity-map identities.
test.each([
  ["%Y-%m-%d", "2028-02-29"], ["%Y.%m.%d", "2028.02.29"], ["%Y/%m/%d", "2028/02/29"],
  ["%d-%m-%Y", "29-02-2028"], ["%d.%m.%Y", "29.02.2028"], ["%d/%m/%Y", "29/02/2028"], ["%m/%d/%Y", "02/29/2028"],
])("date uses declared shipped format %s and retains the calendar date across zones", (date_format, displayed) => {
  const configured = { ...inZone("Pacific/Kiritimati"), date_format };
  expect(formatEditorValue("date", "2028-02-29", configured)).toBe(displayed);
  expect(parseEditorValue("date", displayed, configured)).toEqual({ ok: true, value: "2028-02-29" });
});

test("date accepts unpadded components and explicit ISO independently of the display format", () => {
  expect(parseEditorValue("date", "1/2/2026", locale)).toEqual({ ok: true, value: "2026-02-01" });
  expect(parseEditorValue("date", "2026-12-31", locale)).toEqual({ ok: true, value: "2026-12-31" });
  const named = { ...locale, date_format: "%d %B %Y" };
  expect(formatEditorValue("date", "2026-11-03", named)).toBe("03 November 2026");
  expect(parseEditorValue("date", "3 November 2026", named)).toEqual({ ok: true, value: "2026-11-03" });
});

test("date rejects malformed values, overflow, non-leap days and empty components without normalization", () => {
  for (const input of ["29/02/2027", "31/04/2026", "00/01/2026", "03//2026", "2026-02-30", "not a date", "03/11/0000"]) invalid(parseEditorValue("date", input, locale), "Enter a valid date.");
  expect(parseEditorValue("date", " ", locale)).toEqual({ ok: true, value: false });
  expect(formatEditorValue("date", false, locale)).toBe("");
  expect(() => formatEditorValue("date", "2026-02-30", locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
  expect(() => formatEditorValue("date", 17 as unknown as string, locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
});

// UTC/zone conversion subset; the parameterized cases are not parity-map identities.
test.each([
  ["America/New_York", "2027-01-01 00:30:45", "31/12/2026 19:30:45"],
  ["Asia/Kathmandu", "2026-12-31 20:30:45", "01/01/2027 02:15:45"],
  ["Australia/Lord_Howe", "2026-06-01 00:00:00", "01/06/2026 10:30:00"],
])("datetime serializes UTC and displays in %s with date/year crossing", (tz, value, displayed) => {
  const configured = inZone(tz);
  expect(formatEditorValue("datetime", value, configured)).toBe(displayed);
  expect(parseEditorValue("datetime", displayed, configured)).toEqual({ ok: true, value });
});

test("datetime never interprets a stored UTC value in the ambient browser timezone", () => {
  expect(partsFromValue("datetime", "2026-11-03 09:15:47", inZone("Asia/Jakarta"))).toEqual({ year: 2026, month: 11, day: 3, hour: 16, minute: 15, second: 47 });
  expect(valueFromParts("datetime", { year: 2026, month: 11, day: 3, hour: 16, minute: 15, second: 47 }, inZone("Asia/Jakarta"))).toEqual({ ok: true, value: "2026-11-03 09:15:47" });
  expect(() => partsFromValue("datetime", "2026-11-03T09:15:47Z", locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented datetime value." }));
});

test("datetime rejects DST holes and newly entered repeated wall times with precise errors", () => {
  const configured = inZone("America/New_York");
  invalid(parseEditorValue("datetime", "08/03/2026 02:30:00", configured), "This local time does not exist in America/New_York.");
  invalid(parseEditorValue("datetime", "01/11/2026 01:30:00", configured), "This local time occurs twice in America/New_York. Enter an ISO datetime with an explicit offset.");
  expect(parseEditorValue("datetime", "2026-11-01T01:30:00-05:00", configured)).toEqual({ ok: true, value: "2026-11-01 06:30:00" });
});

test("unchanged displayed datetime preserves either existing repeated-hour instant and hidden seconds", () => {
  const configured = inZone("America/New_York");
  for (const previous of ["2026-11-01 05:30:47", "2026-11-01 06:30:47"]) {
    const displayed = formatEditorValue("datetime", previous, configured, { showSeconds: false });
    expect(displayed).toBe("01/11/2026 01:30");
    expect(parseEditorValue("datetime", displayed, configured, { previous, showSeconds: false })).toEqual({ ok: true, value: previous });
  }
  expect(parseEditorValue("datetime", "03/11/2026 09:16", locale, { previous: "2026-11-03 09:15:47", showSeconds: false })).toEqual({ ok: true, value: "2026-11-03 09:16:47" });
});

test("datetime supports a 12-hour configured time format and rejects invalid time components", () => {
  const configured = { ...locale, time_format: "%I:%M:%S %p" };
  expect(formatEditorValue("datetime", "2026-11-03 00:15:00", configured)).toBe("03/11/2026 12:15:00 am");
  expect(parseEditorValue("datetime", "03/11/2026 12:15:00 PM", configured)).toEqual({ ok: true, value: "2026-11-03 12:15:00" });
  for (const input of ["03/11/2026 24:01:00", "03/11/2026 12:60:00", "03/11/2026 12:15:60"]) invalid(parseEditorValue("datetime", input, locale), "Enter a valid date and time.");
});

/// (partial: time parser, formatting and suggestion values, not UI navigation)
test("time respects 12/24-hour formats, preserves hidden seconds and distinguishes invalid typing", () => {
  expect(formatTimeValue("17:05:47", locale)).toBe("17:05");
  expect(formatTimeValue("17:05:47", locale, true)).toBe("17:05:47");
  const twelve = { ...locale, time_format: "%I:%M:%S %p" };
  expect(formatTimeValue("00:05:00", twelve)).toBe("12:05 am");
  expect(parseTimeValue("12:05 PM", twelve)).toEqual({ ok: true, value: "12:05:00" });
  expect(parseTimeValue("17:06", locale, { previous: "17:05:47" })).toEqual({ ok: true, value: "17:06:47" });
  expect(parseTimeValue("17:06:12", locale, { showSeconds: true })).toEqual({ ok: true, value: "17:06:12" });
  for (const input of ["24:01", "25:00", "12:60", "17:01:60", "words"]) invalid(parseTimeValue(input, locale, { showSeconds: true }), "Enter a valid time.");
  expect(parseTimeValue("", locale)).toEqual({ ok: true, value: false });
});

test("time rounding carries hour and midnight boundaries to a valid canonical time", () => {
  expect(roundTimeValue("12:58:17", 5)).toBe("13:00:17");
  expect(roundTimeValue("23:59:47", 5)).toBe("00:00:47");
  expect(parseTimeValue("12:58", locale, { rounding: 5 })).toEqual({ ok: true, value: "13:00:00" });
  expect(timeSuggestions(5)).toHaveLength(96);
  expect(timeSuggestions(5).at(-1)).toBe("23:45:00");
  expect(timeSuggestions(30)).toHaveLength(48);
  expect(new Set(timeSuggestions(0)).size).toBe(timeSuggestions(0).length);
  for (const rounding of [-1, 61, 1.5, NaN]) expect(() => timeSuggestions(rounding)).toThrowError(expect.objectContaining({ kind: "validation", message: "Time rounding must be an integer from 0 to 60 minutes." }));
});

test("conversion bounds untrusted text and configuration before parsing", () => {
  invalid(parseEditorValue("datetime", "x".repeat(257), locale), "Date/time input exceeds 256 characters.");
  invalid(parseTimeValue("x".repeat(257), locale), "Date/time input exceeds 256 characters.");
  expect(() => editorLocale({ ...locale, date_format: "x".repeat(129) })).toThrowError(expect.objectContaining({ kind: "validation", message: "Date/time format exceeds 128 characters." }));
  expect(() => editorLocale({ ...locale, date_format: "%Y-%m-%d%" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Incomplete date/time format directive." }));
  invalid(valueFromParts("date", { year: 2026, month: 2, day: 30, hour: 0, minute: 0, second: 0 }, locale), "Enter a valid date.");
  expect(partsFromValue("date", false, locale)).toBeNull();
});

test("format admission refuses repeated semantic fields before constructing a parser", () => {
  expect(() => editorLocale({ ...locale, date_format: "%d".repeat(50) + "%m%Y" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Repeated date/time format component: day." }));
  expect(() => editorLocale({ ...locale, date_format: "%Y-%y-%m-%d" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Repeated date/time format component: year." }));
  expect(() => editorLocale({ ...locale, date_format: "%Y-%m-%B-%d" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Repeated date/time format component: month." }));
  expect(() => editorLocale({ ...locale, time_format: "%H %I:%M %p" })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Repeated date/time format component: hour." }));
});

test("rounding refuses a false or malformed runtime value with a precise represented-value error", () => {
  for (const value of [false, null, 17, "12:99:00"]) expect(() => roundTimeValue(value as unknown as string, 5)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented time value." }));
});

test("represented wire values require an exact canonical match including the final character", () => {
  for (const suffix of ["\n", "\r\n"]) {
    expect(() => formatEditorValue("date", "2026-11-03" + suffix, locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented date value." }));
    expect(() => partsFromValue("datetime", "2026-11-03 09:15:47" + suffix, locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented datetime value." }));
    expect(() => formatTimeValue("09:15:47" + suffix, locale)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid represented time value." }));
  }
});

test("public conversion options reject null, unknown members and wrong scalar types explicitly", () => {
  for (const options of [null, 0, { ignore: true }]) {
    expect(() => parseEditorValue("date", "03/11/2026", locale, options as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid date/time conversion options." }));
    expect(() => parseTimeValue("09:15", locale, options as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid date/time conversion options." }));
    expect(() => formatEditorValue("date", "2026-11-03", locale, options as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid date/time conversion options." }));
  }
  expect(() => parseEditorValue("date", "03/11/2026", locale, { showSeconds: 1 } as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid seconds visibility option." }));
  expect(() => parseTimeValue("09:15", locale, { rounding: null } as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Time rounding must be an integer from 0 to 60 minutes." }));
});

test("an explicit ISO offset without seconds preserves represented hidden seconds", () => {
  const previous = "2026-11-03 09:15:47";
  expect(parseEditorValue("datetime", "2026-11-03T09:16Z", locale, { previous, showSeconds: false })).toEqual({ ok: true, value: "2026-11-03 09:16:47" });
  expect(parseEditorValue("datetime", "2026-11-03T09:16:12Z", locale, { previous, showSeconds: false })).toEqual({ ok: true, value: "2026-11-03 09:16:12" });
});

test("non-divisor minute rounding keeps hour-based suggestions stable", () => {
  expect(roundTimeValue("01:00:00", 7)).toBe("01:00:00");
  expect(roundTimeValue("01:59:00", 7)).toBe("01:56:00");
  expect(parseTimeValue("01:00", locale, { rounding: 7 })).toEqual({ ok: true, value: "01:00:00" });
});


test("date conversion ignores an unsupported unused time format while strict locale admission still refuses it", () => {
  const configured = { ...locale, time_format: "%f" };
  expect(() => editorLocale(configured)).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %f." }));
  expect(formatEditorValue("date", "2026-11-03", configured)).toBe("03/11/2026");
  expect(parseEditorValue("date", "04/11/2026", configured)).toEqual({ ok: true, value: "2026-11-04" });
});

test("time conversion ignores an unsupported unused date format while strict locale admission still refuses it", () => {
  const configured = { ...locale, date_format: "%j" };
  expect(() => editorLocale(configured)).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unsupported date/time format directive: %j." }));
  expect(formatTimeValue("10:15:00", configured)).toBe("10:15");
  expect(parseTimeValue("11:20", configured)).toEqual({ ok: true, value: "11:20:00" });
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";
import { isAddressSizeRefusal, parseRoute, routeUrl, type Route, decimalId } from "../src/client/route.js";

interface ContractCase {
  name: string;
  input: string;
  canonical: string | null;
  error_key: string | null;
  browser: boolean;
}
// Like the other source-contract suites, Vitest runs with the web workspace as cwd.
const corpus = JSON.parse(readFileSync(resolve(process.cwd(), "../testdata/shell-url-contract.json"), "utf8")) as { schema: number; cases: ContractCase[] };
const invalid = (key: string) => expect.objectContaining({ kind: "validation", message: `Invalid navigation parameter: ${key}.` });

/// (partial: browser transport canonicalization from the shared corpus)
/// (partial: browser admission and stable refusal from the shared corpus)
test("browser consumes every applicable shared shell URL contract case", () => {
  expect(corpus.schema).toBe(1);
  for (const entry of corpus.cases.filter(entry => entry.browser)) {
    const roundTrip = () => routeUrl(parseRoute(new URL(entry.input, "https://erp.example.test")));
    if (entry.error_key !== null) {
      expect(roundTrip, entry.name).toThrowError(invalid(entry.error_key));
    } else {
      const canonical = roundTrip();
      expect(canonical, entry.name).toBe(entry.canonical);
      expect(routeUrl(parseRoute(new URL(canonical, "https://erp.example.test"))), entry.name).toBe(canonical);
    }
  }
});

/// (partial: serializer validates identifiers, state consistency and company bounds)
test("serializer refuses invalid route objects before publishing a URL", () => {
  const cases: { route: Route; key: string }[] = [
    { route: { action: "01" }, key: "action" },
    { route: { action: "42", menu: "0" }, key: "menu" },
    { route: { action: "42", active_id: "9223372036854775808" }, key: "active_id" },
    { route: { action: "42", id: "7" }, key: "view_type" },
    { route: { action: "42", id: "01", view_type: "form" }, key: "id" },
    { route: { search: "book" }, key: "action" },
    { route: { action: "42", model: "Res.Users" }, key: "model" },
    { route: { action: "42", model: "a".repeat(129) }, key: "model" },
    { route: { action: "42", method_action: "1" }, key: "method_action" },
    { route: { cids: [] }, key: "cids" },
    { route: { cids: ["1", "1"] }, key: "cids" },
    { route: { cids: ["0"] }, key: "cids" },
    { route: { cids: Array.from({ length: 65 }, (_, index) => String(index + 1)) }, key: "cids" },
  ];
  for (const { route, key } of cases) expect(() => routeUrl(route)).toThrowError(invalid(key));
});

/// (partial: serializer refuses wrong runtime types at its transport boundary)
test("serializer refuses runtime values outside the Route type", () => {
  // Addon JavaScript can call this API without TypeScript; these casts deliberately exercise that boundary.
  const cases: { route: unknown; key: string }[] = [
    { route: { action: 42 }, key: "action" },
    { route: { action: "42", search: 7 }, key: "search" },
    { route: { action: "42", model: null }, key: "model" },
    { route: { action: "42", view_type: "kanban" }, key: "view_type" },
    { route: { debug: "2" }, key: "debug" },
    { route: { cids: "1,2" }, key: "cids" },
    { route: { cids: [1] }, key: "cids" },
    { route: { action: "42", domain: "[]" }, key: "url" },
  ];
  for (const { route, key } of cases) expect(() => routeUrl(route as Route)).toThrowError(invalid(key));
});

/// (partial: serializer rejects empty and unsafe decoded transport values)
test("serializer applies closed string admission to every opaque state parameter", () => {
  for (const key of ["search", "search_facets", "search_filters", "search_groupby", "list_tab"] as const) {
    for (const value of ["", "line\nfeed", "back\\slash", "//example", "\u007f", "\ud800"]) {
      expect(() => routeUrl({ action: "42", [key]: value })).toThrowError(invalid(key));
    }
  }
});

/// (partial: serializer exact encoded URI safety cap and expansion)
test("serializer counts the complete canonical encoded URI at the 2048 byte boundary", () => {
  const prefix = "/erp/action-42?search=";
  const room = 2048 - prefix.length;
  const boundary = { action: "42", search: "a".repeat(room) };
  expect(routeUrl(boundary)).toBe(prefix + boundary.search);
  expect(() => routeUrl({ ...boundary, search: boundary.search + "a" })).toThrowError(invalid("url"));
  const expanded = { action: "42", search: ",".repeat(Math.floor(room / 3)) + "a".repeat(room % 3) };
  expect(routeUrl(expanded)).toBe(prefix + "%2C".repeat(Math.floor(room / 3)) + "a".repeat(room % 3));
  expect(() => routeUrl({ ...expanded, search: expanded.search + "書" })).toThrowError(invalid("url"));
});

/// (partial: an empty fragment is still outside the closed URL grammar)
test("browser rejects a fragment delimiter even when its value is empty", () => {
  expect(() => parseRoute(new URL("/erp#", "https://erp.example.test"))).toThrowError(invalid("hash"));
});

/// (partial: unknown parameter names and values never enter browser diagnostics)
test("browser diagnostics name only closed navigation parameters", () => {
  const marker = "shell_url_private_marker";
  const hostileKeys = [marker, `<${marker}>`, `line\n${marker}`, "__proto__", "constructor"];
  for (const key of hostileKeys) {
    const attempts = [
      () => parseRoute(new URL(`/erp/action-42?${encodeURIComponent(key)}=${marker}`, "https://erp.example.test")),
      () => routeUrl({ action: "42", [key]: marker } as Route),
      () => decimalId("0", key),
    ];
    for (const attempt of attempts) {
      expect(attempt).toThrowError(invalid("url"));
      try { attempt(); } catch (error) {
        expect((error as Error).message).not.toContain(marker);
        expect((error as Error).message).not.toContain(key);
      }
    }
  }
  expect(() => parseRoute(new URL(`/erp?debug=${marker}`, "https://erp.example.test")))
    .toThrowError(invalid("debug"));
});

/// (partial: browser refuses a renderer that only the server registry admits)
test("browser refuses registered-server-only renderers explicitly", () => {
  expect(() => parseRoute(new URL("/erp/action-42?view_type=addon_view", "https://erp.example.test")))
    .toThrowError(invalid("view_type"));
});


/// (partial: the address tells a state that is too large for it from a malformed parameter, so a caller can word the first as size)
test("a value or an address past its length is a size refusal, and a malformed pivot parameter is not", () => {
  const base = parseRoute(new URL("/erp/action-20?view_type=pivot", "https://erp.example.test"));
  const refusal = (extra: Partial<Route>): unknown => { try { routeUrl({ ...base, ...extra }); } catch (error) { return error; } throw new Error(`admitted: ${JSON.stringify(extra).slice(0, 80)}`); };
  const open = (count: number): string => JSON.stringify({ r: Array.from({ length: count }, (_, index) => [`k${index}`]) });
  expect(open(400).length).toBeGreaterThan(2048);
  expect(isAddressSizeRefusal(refusal({ pivot_open: open(400) }))).toBe(true); // one value past 2,048 characters
  expect(open(150).length).toBeLessThan(2048);
  expect(isAddressSizeRefusal(refusal({ pivot_open: open(150) }))).toBe(true); // each value fits, the encoded address does not
  expect(isAddressSizeRefusal(refusal({ pivot_open: JSON.stringify({ r: [["x".repeat(300)]] }) }))).toBe(true); // one header value past 256 characters
  expect(isAddressSizeRefusal(refusal({ pivot_open: JSON.stringify({ c: [Array.from({ length: 17 }, () => 1)] }) }))).toBe(true); // a header deeper than 16
  expect(isAddressSizeRefusal(refusal({ pivot_sort: JSON.stringify(["__count", "asc", ["x".repeat(300)]]) }))).toBe(true); // a sort under such a header
  expect(isAddressSizeRefusal(refusal({ pivot_sort: '["__count","sideways",[]]' }))).toBe(false);
  for (const extra of [{ pivot_open: '{"x":[]}' }, { pivot_open: "not json" }, { pivot_rows: "priority,priority" }, { pivot_rows: "Bad Field" }] as Partial<Route>[]) {
    const error = refusal(extra);
    expect(error, JSON.stringify(extra)).toEqual(expect.objectContaining({ kind: "validation" }));
    expect(isAddressSizeRefusal(error), JSON.stringify(extra)).toBe(false);
  }
  expect(isAddressSizeRefusal(new Error("Invalid navigation parameter: url."))).toBe(false); // the text is not the signal
});

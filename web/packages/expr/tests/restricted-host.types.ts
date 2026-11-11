/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compile-only contract tests, included by web/tsconfig.json through packages/**/*.ts.
import type {
  RestrictedCardHostV1,
  RestrictedDateTime,
  RestrictedJsonValue,
} from "@erpforall/expr/restricted-host";

type Fields = { title: string; total: number; tags: readonly string[] };
type Context = { lang: string; active_ids: readonly number[] };
type Helpers = { badge: (label: string, total: number) => string };
declare const host: RestrictedCardHostV1<Fields, Context, Helpers>;
const title: string = host.record.title.value;
const rawTotal: number = host.record.total.raw_value;
const fieldName: "title" = host.record.title.name;
const lang: string = host.context.lang;
const id: number | false = host.record.id.raw_value;
const formattedId: number | false = host.record.id.value;
const editable: boolean = host.widget.editable;
const selection: boolean = host.selection_mode;
const readOnly: boolean = host.read_only_mode;
const badge: string = host.badge(title, rawTotal);
const parsed: unknown = host.JSON.parse("[]");
const text: string = host.JSON.stringify({ title, total: rawTotal });
const magnitude: number = host.Math.abs(rawTotal);
const isArray: boolean = host.Array.isArray(host.record.tags.raw_value);
const date: RestrictedDateTime = host.luxon.DateTime.fromISO("2026-10-31");
const day: number = date.startOf("day").diff(host.luxon.DateTime.now(), "days").days;
const formatted: string = date.toFormat("yyyy-MM-dd");
const locale: string = date.toLocaleString({ month: "short", day: "numeric" });
const image: string = host.kanban_image("res.partner", "image_128", id);
const colour: string = host.kanban_color(2);
const version: 1 = host.api_version;
void [title, rawTotal, fieldName, lang, id, formattedId, editable, selection, readOnly, badge, parsed,
  text, magnitude, isArray, day, formatted, locale, image, colour, version];

// Adding a declared field/helper preserves all V1 members and prior consumer types.
declare const additive: RestrictedCardHostV1<Fields & { note: string }, Context, Helpers & {
  count: (values: readonly number[]) => number;
}>;
const existingConsumer: RestrictedCardHostV1<Fields, Context, Helpers> = additive;
void existingConsumer;
void additive.count([1, 2]);

// @ts-expect-error unknown fields have no implicit index signature
void host.record.secret;
// @ts-expect-error unknown context keys have no implicit index signature
void host.context.token;
// @ts-expect-error undeclared host functions are unavailable
host.eval("alert(1)");
// @ts-expect-error helper argument shapes are retained
host.badge(3, "wrong");
// @ts-expect-error immutable field wrapper
host.record.title.value = "changed";
// @ts-expect-error nested arrays are immutable
host.context.active_ids.push(1);
// @ts-expect-error namespaces are immutable
host.JSON.parse = () => null;
// @ts-expect-error constructor is not a callable capability
host.constructor("return globalThis")();
// @ts-expect-error prototype does not expose data
const prototypeLeak: string = host.record.__proto__;
// @ts-expect-error random is not a pure Math capability
host.Math.random();
// @ts-expect-error Luxon members are explicitly listed
host.luxon.DateTime.fromJSDate(new Date());
// @ts-expect-error dates expose no arbitrary mutation methods
host.luxon.DateTime.now().set({ year: 2000 });
// @ts-expect-error JSON.parse does not assert an unchecked result type
const unchecked: string = host.JSON.parse('"value"');
// @ts-expect-error wrong version cannot substitute the V1 contract
const wrongVersion: RestrictedCardHostV1<Fields, Context, Helpers> = { ...host, api_version: 2 };
void [prototypeLeak, unchecked, wrongVersion];

// Builtin names remain authoritative when an addon tries to register a collision.
declare const collision: RestrictedCardHostV1<Fields, Context, { Math: () => string }>;
// @ts-expect-error host helper may not replace a builtin namespace
void collision.Math;
declare const prototypeHelper: RestrictedCardHostV1<Fields, Context, { constructor: () => string }>;
// @ts-expect-error prototype names cannot be registered as helpers
void prototypeHelper.constructor;
declare const dataHelper: RestrictedCardHostV1<Fields, Context, { label: string }>;
// @ts-expect-error a helper declaration must be a function, not data
const invalidDataHelper: string = dataHelper.label();
void invalidDataHelper;

// Broad key domains must not turn the host allowlist into an index signature.
declare const stringFields: RestrictedCardHostV1<Record<string, string>>;
// @ts-expect-error a broad field map cannot expose undeclared fields
void stringFields.record.secret;
declare const numberFields: RestrictedCardHostV1<Record<number, string>>;
// @ts-expect-error a broad numeric field map is not a closed field set
void numberFields.record[42];
declare const symbolFields: RestrictedCardHostV1<Record<symbol, string>>;
declare const arbitrarySymbol: symbol;
// @ts-expect-error a broad symbol field map is not a closed field set
void symbolFields.record[arbitrarySymbol];
declare const stringContext: RestrictedCardHostV1<Fields, Record<string, string>>;
// @ts-expect-error a broad context map cannot expose undeclared keys
void stringContext.context.secret;
declare const numberContext: RestrictedCardHostV1<Fields, Record<number, string>>;
// @ts-expect-error a broad numeric context map is not closed
void numberContext.context[42];
declare const symbolContext: RestrictedCardHostV1<Fields, Record<symbol, string>>;
// @ts-expect-error a broad symbol context map is not closed
void symbolContext.context[arbitrarySymbol];
declare const stringHelpers: RestrictedCardHostV1<Fields, Context, Record<string, () => string>>;
// @ts-expect-error a broad helper map cannot grant arbitrary callable names
void stringHelpers.eval;
declare const numberHelpers: RestrictedCardHostV1<Fields, Context, Record<number, () => string>>;
// Indexed access on never itself is permitted by TypeScript; assert rejection directly.
type AssertNever<T extends never> = T;
type RejectedNumberHelpers = AssertNever<typeof numberHelpers>;
declare const symbolHelpers: RestrictedCardHostV1<Fields, Context, Record<symbol, () => string>>;
type RejectedSymbolHelpers = AssertNever<typeof symbolHelpers>;

declare const nestedField: RestrictedCardHostV1<{ payload: { dict: Record<string, string> } }>;
// @ts-expect-error nested field data must not smuggle in an open dictionary
void nestedField.record.payload.raw_value.dict.secret;
declare const nestedContext: RestrictedCardHostV1<Fields, { user: { flags: Record<string, boolean> } }>;
// @ts-expect-error nested context data must retain declared keys
void nestedContext.context.user.flags.secret;
declare const helperResult: RestrictedCardHostV1<Fields, Context, {
  payload: () => { dict: Record<string, string> };
}>;
// @ts-expect-error helper results do not grant an arbitrary dictionary capability
void helperResult.payload().dict.secret;

// Open dictionaries remain explicit JSON data, not a host member namespace.
declare const jsonDictionary: Readonly<Record<string, RestrictedJsonValue>>;
const encodedDictionary: string = host.JSON.stringify(jsonDictionary);
void encodedDictionary;

declare const helperArgument: RestrictedCardHostV1<Fields, Context, {
  inspect: (input: { dict: Record<string, string> }) => string;
}>;
// @ts-expect-error helper arguments do not smuggle in a nested dictionary
helperArgument.inspect({ dict: { secret: "x" } });

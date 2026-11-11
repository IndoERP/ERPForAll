/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Type-only, additions-only V1 contract for restricted card templates.
 * These declarations are not a sandbox: owns member admission, runtime membranes,
 * JSON validation, quotas and markup/URL sanitisation. No evaluator or browser global is exported.
 */

/** Data accepted by the JSON host capability; functions and undefined are excluded. */
export type RestrictedJsonValue =
  | null | boolean | number | string
  | readonly RestrictedJsonValue[]
  | { readonly [key: string]: RestrictedJsonValue };

// Test the domain, not Extract<keyof T, Reserved>: Extract<string, a literal> is never.
// Thus an open map cannot pass the same collision check used for a finite set of named keys.
type HasBroadKeys<T extends object> =
  string extends keyof T ? true :
  number extends keyof T ? true :
  symbol extends keyof T ? true : false;

/** Deep readonly data; open dictionaries are admitted only by the explicit JSON capability. */
export type RestrictedData<T> =
  T extends (...args: never[]) => unknown ? never :
  T extends null | boolean | number | string ? T :
  T extends readonly (infer Item)[] ? readonly RestrictedData<Item>[] :
  T extends object ? HasBroadKeys<T> extends true ? never
    : { readonly [Key in keyof T]: RestrictedData<T[Key]> } : never;

// TypeScript ordinarily supplies Object members on structural object types. Mark them unusable;
// the compiler/runtime member allowlist must reject even reading these names in template source.
type NoPrototype = {
  readonly constructor?: never;
  readonly __proto__?: never;
  readonly toString?: never;
  readonly toLocaleString?: never;
  readonly valueOf?: never;
  readonly hasOwnProperty?: never;
  readonly isPrototypeOf?: never;
  readonly propertyIsEnumerable?: never;
};

/** A declared card field exposes formatted text, raw data and its own field name. */
export type RestrictedCardField<Raw, Name extends string> = NoPrototype & {
  readonly value: string;
  readonly raw_value: RestrictedData<Raw>;
  readonly name: Name;
};

/** A closed record shape; id and context cannot be replaced by addon field names. */
export type RestrictedCardRecord<Fields extends object, Context extends object> =
  true extends HasBroadKeys<Fields> | HasBroadKeys<Context> ? never :
  Extract<keyof Fields, "id" | "context" | keyof NoPrototype> extends never
    ? NoPrototype & {
      readonly id: NoPrototype & {
        readonly value: number | false;
        readonly raw_value: number | false;
      };
      readonly context: RestrictedData<Context>;
    } & { readonly [Name in keyof Fields]: RestrictedCardField<Fields[Name], Extract<Name, string>> }
    : never;

/** Date/time units made available by the accepted restricted-host design. */
export type RestrictedDateUnit =
  | "year" | "quarter" | "month" | "week" | "day" | "hour" | "minute" | "second" | "millisecond";
/** Duration units accepted by arithmetic and diff. */
export type RestrictedDurationUnit =
  | "years" | "quarters" | "months" | "weeks" | "days" | "hours" | "minutes" | "seconds" | "milliseconds";
/** Closed duration data for plus/minus; the host validates actual values. */
export type RestrictedDurationValues = Readonly<Partial<Record<RestrictedDurationUnit, number>>>;
/** The listed Luxon duration result, without the Luxon prototype or arbitrary methods. */
export type RestrictedDuration = NoPrototype & {
  readonly years: number;
  readonly quarters: number;
  readonly months: number;
  readonly weeks: number;
  readonly days: number;
  readonly hours: number;
  readonly minutes: number;
  readonly seconds: number;
  readonly milliseconds: number;
  readonly as: (unit: RestrictedDurationUnit) => number;
  readonly toFormat: (format: string) => string;
};
/** Explicit Luxon instance capabilities from ui-framework.md §4.2. */
export type RestrictedDateTime = Omit<NoPrototype, "toLocaleString"> & {
  readonly ts: number;
  readonly year: number;
  readonly month: number;
  readonly day: number;
  readonly weekday: number;
  readonly isValid: boolean;
  readonly toFormat: (format: string) => string;
  readonly toLocaleString: (options?: Readonly<Intl.DateTimeFormatOptions>) => string;
  readonly startOf: (unit: RestrictedDateUnit) => RestrictedDateTime;
  readonly endOf: (unit: RestrictedDateUnit) => RestrictedDateTime;
  readonly diff: (other: RestrictedDateTime, unit: RestrictedDurationUnit | readonly RestrictedDurationUnit[]) => RestrictedDuration;
  readonly plus: (duration: RestrictedDurationValues) => RestrictedDateTime;
  readonly minus: (duration: RestrictedDurationValues) => RestrictedDateTime;
  readonly toISODate: () => string | null;
};
/** Explicit construction options; no system settings or plugin handles are exposed. */
export type RestrictedDateTimeOptions = {
  readonly zone?: string;
  readonly setZone?: boolean;
  readonly locale?: string;
  readonly outputCalendar?: string;
  readonly numberingSystem?: string;
};
/** Restricted Luxon construction namespace. Duration instances come from diff. */
export type RestrictedLuxon = NoPrototype & {
  readonly DateTime: NoPrototype & {
    readonly fromISO: (text: string, options?: RestrictedDateTimeOptions) => RestrictedDateTime;
    readonly fromSQL: (text: string, options?: RestrictedDateTimeOptions) => RestrictedDateTime;
    readonly local: () => RestrictedDateTime;
    readonly now: () => RestrictedDateTime;
  };
};

/** Explicit pure Math list: future lib.d.ts members do not widen the host API automatically. */
export type RestrictedMath = NoPrototype & Readonly<Pick<Math,
  | "E" | "LN10" | "LN2" | "LOG10E" | "LOG2E" | "PI" | "SQRT1_2" | "SQRT2"
  | "abs" | "acos" | "acosh" | "asin" | "asinh" | "atan" | "atanh" | "atan2"
  | "cbrt" | "ceil" | "clz32" | "cos" | "cosh" | "exp" | "expm1" | "floor"
  | "fround" | "hypot" | "imul" | "log" | "log1p" | "log10" | "log2" | "max"
  | "min" | "pow" | "round" | "sign" | "sin" | "sinh" | "sqrt" | "tan" | "tanh" | "trunc"
>>;

type HostBase<Fields extends object, Context extends object> = NoPrototype & {
  readonly api_version: 1;
  readonly record: RestrictedCardRecord<Fields, Context>;
  readonly context: RestrictedData<Context>;
  readonly widget: NoPrototype & {
    readonly editable: boolean;
    readonly deletable: boolean;
    readonly color_mode: boolean;
  };
  readonly selection_mode: boolean;
  readonly read_only_mode: boolean;
  readonly luxon: RestrictedLuxon;
  readonly JSON: NoPrototype & {
    readonly parse: (text: string) => unknown;
    readonly stringify: (value: RestrictedJsonValue) => string;
  };
  readonly Math: RestrictedMath;
  readonly Array: NoPrototype & { readonly isArray: (value: unknown) => boolean };
  readonly String: (value: RestrictedJsonValue) => string;
  readonly Number: (value: RestrictedJsonValue) => number;
  readonly Boolean: (value: RestrictedJsonValue) => boolean;
  readonly parseInt: (text: string, radix?: number) => number;
  readonly parseFloat: (text: string) => number;
  readonly activity_image: (model: string, field: string, id: number | false) => string;
  readonly kanban_image: (model: string, field: string, id: number | false) => string;
  readonly kanban_getcolor: (value: number | string) => number;
  readonly kanban_color: (value: number | string) => string;
  readonly getFormattedValue: (fieldId: string) => string;
};

type HostHelpers<Helpers extends object> = {
  readonly [Name in keyof Helpers]: Helpers[Name] extends (...args: infer Args) => infer Result
    ? (...args: { [Index in keyof Args]: RestrictedData<Args[Index]> }) => RestrictedData<Result>
    : never;
};

/**
 * Closed V1 card context. Addons name each helper and field in their own shape; arbitrary names,
 * helper collisions with builtins and prototype names are rejected. Future additions preserve V1.
 * Helpers are code registered by an addon, never functions decoded from template data.
 */
export type RestrictedCardHostV1<
  Fields extends object = Record<never, never>,
  Context extends object = Record<never, never>,
  Helpers extends object = Record<never, never>,
> = true extends HasBroadKeys<Fields> | HasBroadKeys<Context> | HasBroadKeys<Helpers> ? never :
  Extract<keyof Helpers, keyof HostBase<Fields, Context>> extends never
  ? HostBase<Fields, Context> & HostHelpers<Helpers>
  : never;

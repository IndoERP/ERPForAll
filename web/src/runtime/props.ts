/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Component props schema definition and universal strict validation.

import { UIError } from "./errors.js";

/** What a validator sees: the value at `path`, a sink for issues, and a way into nested values. */
export interface ValidationContext {
  readonly value: unknown;
  readonly path: readonly (string | number)[];
  readonly addIssue: (message: string) => void;
  readonly withKey: (key: string | number) => ValidationContext;
}

export type Validator = (context: ValidationContext) => void;

/**
 * A prop type: a validator with its modifiers. Immutable (review round 3, R19): every `TypeDef` the
 * runtime builds is frozen, so a schema cannot be changed after it was declared.
 */
export interface TypeDef {
  (context: ValidationContext): void;
  readonly isOptional?: boolean;
  readonly defaultFactory?: () => unknown;
  readonly optional: (defaultFactory?: () => unknown) => TypeDef;
  readonly shape?: Readonly<Record<string, TypeDef>> | readonly TypeDef[];
}

const SCHEMA_BRAND: unique symbol = Symbol("erp.propsSchema");

/** A component's props schema (`static props = props({...})`); `S` keeps the declared keys. */
export interface PropsSchema<S extends Record<string, TypeDef> = Record<string, TypeDef>> {
  readonly [SCHEMA_BRAND]: true;
  readonly shape: Readonly<S>;
}

/** The kind of a value as an issue names it: `null` and `array` by name, `typeof` otherwise. */
function kindOf(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  return typeof value;
}

/**
 * A literal value as an issue shows it. `JSON.stringify` throws on a cycle or a BigInt and returns
 * undefined for a function or a symbol; those are shown by their tag or `String` instead, so formatting
 * an issue never replaces the props error with a `TypeError` (review round 3, R16).
 */
function show(value: unknown): string {
  if (typeof value === "bigint") return `${value}n`;
  if (typeof value === "symbol" || typeof value === "function" || value === undefined) return String(value);
  try {
    return JSON.stringify(value);
  } catch {
    // Only a cycle reaches here (BigInts were handled above): the value's tag is the description.
    return Object.prototype.toString.call(value);
  }
}

function createContext(
  issues: string[],
  value: unknown,
  path: readonly (string | number)[],
): ValidationContext {
  return {
    value,
    path,
    addIssue(msg: string) {
      const p = path.length > 0 ? path.join(".") : "(root)";
      issues.push(`at ${p}: ${msg}`);
    },
    withKey(key: string | number) {
      const nextVal =
        value !== null && typeof value === "object"
          ? (value as Record<string | number, unknown>)[key]
          : undefined;
      return createContext(issues, nextVal, [...path, key]);
    },
  };
}

/** A frozen TypeDef from its validator and modifiers. */
function typeDef(
  check: (context: ValidationContext) => void,
  validate: Validator,
  extra: { isOptional?: boolean; defaultFactory?: () => unknown; shape?: TypeDef["shape"] },
): TypeDef {
  const fn = (context: ValidationContext): void => check(context);
  const optional = (defaultFactory?: () => unknown): TypeDef =>
    typeDef(
      (context) => {
        if (context.value !== undefined) validate(context);
      },
      validate,
      { isOptional: true, defaultFactory, shape: extra.shape },
    );
  return Object.freeze(Object.assign(fn, { optional }, extra)) as TypeDef;
}

function makeType(validate: Validator, shape?: TypeDef["shape"]): TypeDef {
  return typeDef(validate, validate, shape === undefined ? {} : { shape });
}

export const types = {
  string: makeType((ctx) => {
    if (typeof ctx.value !== "string") {
      ctx.addIssue(`expected string, received ${kindOf(ctx.value)}`);
    }
  }),

  number: makeType((ctx) => {
    if (typeof ctx.value !== "number" || Number.isNaN(ctx.value)) {
      ctx.addIssue(`expected number, received ${kindOf(ctx.value)}`);
    }
  }),

  boolean: makeType((ctx) => {
    if (typeof ctx.value !== "boolean") {
      ctx.addIssue(`expected boolean, received ${kindOf(ctx.value)}`);
    }
  }),

  any: makeType(() => {}),

  function: makeType((ctx) => {
    if (typeof ctx.value !== "function") {
      ctx.addIssue(`expected function, received ${kindOf(ctx.value)}`);
    }
  }),

  literal(expected: unknown): TypeDef {
    return makeType((ctx) => {
      if (ctx.value !== expected) {
        ctx.addIssue(`expected literal ${show(expected)}, received ${show(ctx.value)}`);
      }
    });
  },

  optional(type: TypeDef, defaultFactory?: () => unknown): TypeDef {
    return type.optional(defaultFactory);
  },

  array(elementType?: TypeDef): TypeDef {
    return makeType((ctx) => {
      if (!Array.isArray(ctx.value)) {
        ctx.addIssue(`expected array, received ${kindOf(ctx.value)}`);
        return;
      }
      if (elementType !== undefined) {
        for (let i = 0; i < ctx.value.length; i++) {
          elementType(ctx.withKey(i));
        }
      }
    });
  },

  object(shape: Record<string, TypeDef>): TypeDef {
    const frozen = Object.freeze({ ...shape });
    return makeType((ctx) => {
      if (ctx.value === null || typeof ctx.value !== "object" || Array.isArray(ctx.value)) {
        ctx.addIssue(`expected object, received ${kindOf(ctx.value)}`);
        return;
      }
      for (const key of Object.keys(frozen)) {
        const fieldType = frozen[key]!;
        const val = (ctx.value as Record<string, unknown>)[key];
        if (val === undefined) {
          if (!fieldType.isOptional) {
            ctx.addIssue(`missing required object property "${key}"`);
          }
        } else {
          fieldType(ctx.withKey(key));
        }
      }
    }, frozen);
  },

  strictObject(shape: Record<string, TypeDef>): TypeDef {
    const frozen = Object.freeze({ ...shape });
    return makeType((ctx) => {
      if (ctx.value === null || typeof ctx.value !== "object" || Array.isArray(ctx.value)) {
        ctx.addIssue(`expected object, received ${kindOf(ctx.value)}`);
        return;
      }
      const allowed = new Set(Object.keys(frozen));
      for (const key of Object.keys(ctx.value)) {
        if (!allowed.has(key)) {
          ctx.addIssue(`object value has unknown keys: [${key}]`);
        }
      }
      for (const key of Object.keys(frozen)) {
        const fieldType = frozen[key]!;
        const val = (ctx.value as Record<string, unknown>)[key];
        if (val === undefined) {
          if (!fieldType.isOptional) {
            ctx.addIssue(`missing required object property "${key}"`);
          }
        } else {
          fieldType(ctx.withKey(key));
        }
      }
    }, frozen);
  },

  record(valType?: TypeDef): TypeDef {
    return makeType((ctx) => {
      if (ctx.value === null || typeof ctx.value !== "object" || Array.isArray(ctx.value)) {
        ctx.addIssue(`expected object, received ${kindOf(ctx.value)}`);
        return;
      }
      if (valType !== undefined) {
        for (const key of Object.keys(ctx.value)) {
          valType(ctx.withKey(key));
        }
      }
    });
  },

  or(...memberTypes: TypeDef[]): TypeDef {
    return makeType((ctx) => {
      for (const member of memberTypes) {
        const subIssues: string[] = [];
        member(createContext(subIssues, ctx.value, ctx.path));
        if (subIssues.length === 0) return; // matched one of the union options
      }
      ctx.addIssue(`value does not match any union alternative`);
    });
  },

  and(...memberTypes: TypeDef[]): TypeDef {
    return makeType((ctx) => {
      for (const member of memberTypes) {
        member(ctx);
      }
    });
  },

  tuple(...elementTypes: TypeDef[]): TypeDef {
    return makeType((ctx) => {
      if (!Array.isArray(ctx.value)) {
        ctx.addIssue(`expected tuple array, received ${kindOf(ctx.value)}`);
        return;
      }
      if (ctx.value.length !== elementTypes.length) {
        ctx.addIssue(`tuple length mismatch: expected ${elementTypes.length}, got ${ctx.value.length}`);
        return;
      }
      for (let i = 0; i < elementTypes.length; i++) {
        elementTypes[i]!(ctx.withKey(i));
      }
    });
  },
};

/** Creates a component props schema; the shape is copied and frozen, its keys kept in the type. */
export function props<const S extends Record<string, TypeDef>>(shape: S): PropsSchema<S> {
  return Object.freeze({
    [SCHEMA_BRAND]: true as const,
    shape: Object.freeze({ ...shape }),
  });
}

export function isPropsSchema(val: unknown): val is PropsSchema {
  return val !== null && typeof val === "object" && (val as PropsSchema)[SCHEMA_BRAND] === true;
}

/** The issues of one prop's value (after its default was applied); appended to `issues`. */
function checkProp(issues: string[], key: string, value: unknown, type: TypeDef): void {
  if (value === undefined) {
    if (!type.isOptional) issues.push(`missing required prop "${key}"`);
    return;
  }
  type(createContext(issues, value, [key]));
}

function invalid(componentName: string, issues: string[]): UIError {
  return new UIError(`Invalid props passed to component ${componentName}:\n  - ${issues.join("\n  - ")}`, "value");
}

/**
 * Validates every prop the schema declares (defaults already applied): a `UIError` with code `value`
 * listing every issue, in all modes. Keys the schema does not declare are not checked.
 */
export function validateProps(componentName: string, values: Readonly<Record<string, unknown>>, schema: PropsSchema): void {
  const issues: string[] = [];
  for (const [key, type] of Object.entries(schema.shape)) checkProp(issues, key, values[key], type);
  if (issues.length > 0) throw invalid(componentName, issues);
}

/** Validates one updated prop (its default already applied) when the schema declares it. */
export function validateProp(componentName: string, schema: PropsSchema, key: string, value: unknown): void {
  if (!Object.prototype.hasOwnProperty.call(schema.shape, key)) return;
  const issues: string[] = [];
  checkProp(issues, key, value, schema.shape[key]!);
  if (issues.length > 0) throw invalid(componentName, issues);
}

/**
 * Validates props against component schema and applies default factories.
 * Fails loudly with UIError across all environments.
 */
export function validateAndApplyPropsDefaults(
  componentName: string,
  rawProps: Record<string, unknown>,
  schema: PropsSchema,
): Record<string, unknown> {
  const result: Record<string, unknown> = { ...rawProps };
  for (const [key, type] of Object.entries(schema.shape)) {
    if (result[key] === undefined && type.defaultFactory !== undefined) result[key] = type.defaultFactory();
  }
  validateProps(componentName, result, schema);
  return result;
}

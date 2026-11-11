/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Argument binding for builtins and methods, the client counterpart of `crates/erp-expr/src/args.rs`:
 * positional and keyword arguments are matched against a signature; unknown keywords, duplicates,
 * missing and surplus arguments are `TypeError`s with the messages Rust produces.
 */
import { typeError } from "./python-error";
import { type ExprValue, pythonTypeOf } from "./values";

/** A builtin signature. */
export interface Sig {
  /** Function name used in messages. */
  readonly name: string;
  /** Parameter names in positional order. */
  readonly params: readonly string[];
  /** The first `required` parameters must be given. */
  readonly required: number;
  /** The first `posOnly` parameters cannot be passed by keyword. */
  readonly posOnly: number;
  /** At most `maxPos` arguments may be passed positionally. */
  readonly maxPos: number;
}

/** A signature without parameters (`weekday()`, `total_seconds()`, ...). */
export function sig0(name: string): Sig {
  return { name, params: [], required: 0, posOnly: 0, maxPos: 0 };
}

/** Slot `i` holds the value of `params[i]`, `undefined` when absent. */
export type Slots = readonly (ExprValue | undefined)[];

/** Binds `args`/`kwargs` to `sig`. */
export function bindSig(sig: Sig, args: readonly ExprValue[], kwargs: Readonly<Record<string, ExprValue>>): Slots {
  if (args.length > sig.maxPos) {
    throw typeError(
      sig.maxPos === 0
        ? `${sig.name}() takes no positional arguments (${args.length} given)`
        : `${sig.name}() takes at most ${sig.maxPos} argument${sig.maxPos === 1 ? "" : "s"} (${args.length} given)`,
    );
  }
  const slots: (ExprValue | undefined)[] = new Array<ExprValue | undefined>(sig.params.length).fill(undefined);
  for (let i = 0; i < args.length && i < slots.length; i++) {
    slots[i] = args[i];
  }
  for (const key of Object.keys(kwargs)) {
    const index = sig.params.indexOf(key);
    if (index < 0 || index < sig.posOnly) {
      throw typeError(`${sig.name}() got an unexpected keyword argument '${key}'`);
    }
    if (slots[index] !== undefined) {
      throw typeError(`${sig.name}() got multiple values for argument '${key}'`);
    }
    slots[index] = kwargs[key];
  }
  for (let i = 0; i < sig.required; i++) {
    if (slots[i] === undefined) {
      throw typeError(`${sig.name}() missing 1 required positional argument: '${sig.params[i]}'`);
    }
  }
  return slots;
}

/** A required slot; absence after `bindSig` is a signature bug, reported as an error. */
export function requiredSlot(sig: Sig, slots: Slots, i: number): ExprValue {
  const v = slots[i];
  if (v === undefined) {
    throw typeError(`${sig.name}() missing 1 required positional argument: '${sig.params[i] ?? "?"}'`);
  }
  return v;
}

/** CPython names keyword parameters in quotes and positional-only ones by number. */
function paramLabel(param: string): string {
  return /^[0-9]*$/.test(param) ? param : `'${param}'`;
}

/** The value of an `int`/`bool` argument, a `TypeError` naming the parameter otherwise. */
export function intArg(fname: string, param: string, v: ExprValue): bigint {
  if (v.kind === "int") return v.value;
  if (v.kind === "bool") return v.value ? 1n : 0n;
  throw typeError(`${fname}() argument ${paramLabel(param)} must be int, not ${pythonTypeOf(v)}`);
}

/** The value of a `str` argument, a `TypeError` naming the parameter otherwise. */
export function strArg(fname: string, param: string, v: ExprValue): string {
  if (v.kind === "str") return v.value;
  throw typeError(`${fname}() argument ${paramLabel(param)} must be str, not ${pythonTypeOf(v)}`);
}

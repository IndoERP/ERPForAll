/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { SLOT } from "./budget";
import { collectItems, iterateItems } from "./iterable";
import { bindSig } from "./args";
import { evalBinary, evalCompare } from "./ops";
import { CommandNamespace } from "./command";
import {
  type Clock,
  createRelativeDelta,
  createTimeDelta,
  DateTimeClass,
  TimeModule,
  timeDeltaAbs,
} from "./dates";
import { PyDict } from "./dict";
import { ascii, intToFloat, reprBudgeted, strBudgeted } from "./format";
import {
  overflowError,
  typeError,
  valueError,
} from "./python-error";
import {
  type ExprValue,
  boolVal,
  callableVal,
  floatVal,
  intVal,
  isTruthy,
  listVal,
  objectVal,
  ownKwarg,
  pythonTypeOf,
  strVal,
  tupleVal,
} from "./values";

function checkI64(n: bigint): bigint {
  if (n < -9223372036854775808n || n > 9223372036854775807n) {
    throw overflowError("integer result does not fit in 64 bits");
  }
  return n;
}

function getExactFraction(x: number): { sign: bigint; num: bigint; den: bigint } | null {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  view.setFloat64(0, x);
  const bits = view.getBigUint64(0);
  const sign = bits >> 63n;
  const exp = (bits >> 52n) & 0x7ffn;
  const frac = bits & 0xfffffffffffffn;
  if (exp === 0x7ffn) return null;
  if (exp === 0n && frac === 0n) return { sign, num: 0n, den: 1n };
  let mantissa = frac;
  let exponent = exp - 1023n - 52n;
  if (exp !== 0n) {
    mantissa |= 1n << 52n;
  } else {
    exponent += 1n;
  }
  let num = mantissa;
  let den = 1n;
  if (exponent >= 0n) {
    num <<= exponent;
  } else {
    den <<= -exponent;
  }
  return { sign, num, den };
}

export function pyRoundExact(x: number, ndigits: number | null): number {
  if (!Number.isFinite(x)) return x;
  // Rust num::round_float bounds decimal precision by the binary64 exponent range.
  if (ndigits !== null && ndigits > 323) return x;
  if (ndigits !== null && ndigits < -308) return x < 0 || Object.is(x, -0) ? -0 : 0;
  if (ndigits !== null && ndigits < 0) {
    const p = Math.pow(10, -ndigits);
    const scaled = x / p;
    const result = pyRoundExact(scaled, null) * p;
    if (!Number.isFinite(result)) throw overflowError("rounded value too large to represent");
    return result;
  }
  const frac = getExactFraction(x);
  if (!frac) return x;
  const p = ndigits === null ? 1n : 10n ** BigInt(ndigits);
  const num = frac.num * p;
  const den = frac.den;
  let q = num / den;
  const r = num % den;
  const twice = r * 2n;
  if (twice > den || (twice === den && q % 2n !== 0n)) {
    q += 1n;
  }
  let res = Number(`${q}e-${ndigits ?? 0}`);
  if (frac.sign === 1n) res = -res;
  return res;
}

function roundInt(val: bigint, ndigits: number | null): bigint {
  if (ndigits === null || ndigits >= 0) return val;
  if (ndigits < -18) return 0n;
  const p = 10n ** BigInt(-ndigits);
  let q = val / p;
  let r = val % p;
  if (r < 0n) {
    r += p;
    q -= 1n;
  }
  const twice = r * 2n;
  if (twice > p || (twice === p && q % 2n !== 0n)) {
    q += 1n;
  }
  return checkI64(q * p);
}

export function createStandardBuiltins(clock?: Clock): Record<string, ExprValue> {
  const builtins: Record<string, ExprValue> = {};

  builtins["int"] = callableVal("int", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) {
      throw typeError(`'${Object.keys(kwargs)[0]}' is an invalid keyword argument for int()`);
    }
    if (args.length === 0) return intVal(0n);
    if (args.length > 2) throw typeError(`int expected at most 2 arguments, got ${args.length}`);
    const val = args[0]!;

    if (val.kind === "bool") return intVal(val.value ? 1n : 0n);
    if (val.kind === "int") return val;
    if (val.kind === "float") {
      if (Number.isNaN(val.value)) throw valueError("cannot convert float NaN to integer");
      if (!Number.isFinite(val.value)) throw overflowError("cannot convert float infinity to integer");
      return intVal(checkI64(BigInt(Math.trunc(val.value))));
    }
    if (val.kind === "str") {
      const trimmed = val.value.trim().replace(/_/g, "");
      if (!/^[+-]?(?:0|[1-9][0-9]*)$/.test(trimmed)) {
        throw valueError(`invalid literal for int() with base 10: '${val.value}'`);
      }
      return intVal(checkI64(BigInt(trimmed)));
    }
    throw typeError(`int() argument must be a string, a bytes-like object or a real number, not '${pythonTypeOf(val)}'`);
  });

  builtins["float"] = callableVal("float", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("float() takes no keyword arguments");
    if (args.length === 0) return floatVal(0.0);
    if (args.length > 1) throw typeError(`float expected at most 1 argument, got ${args.length}`);
    const val = args[0]!;

    if (val.kind === "bool") return floatVal(val.value ? 1.0 : 0.0);
    if (val.kind === "int") return floatVal(intToFloat(val.value));
    if (val.kind === "float") return val;
    if (val.kind === "str") {
      const trimmed = val.value.trim().replace(/_/g, "");
      const lower = trimmed.toLowerCase();
      if (lower === "nan") return floatVal(NaN);
      if (lower === "inf" || lower === "+inf") return floatVal(Infinity);
      if (lower === "-inf") return floatVal(-Infinity);
      if (!/^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$/.test(trimmed)) {
        throw valueError(`could not convert string to float: '${val.value}'`);
      }
      const n = Number(trimmed);
      return floatVal(n);
    }
    throw typeError(`float() argument must be a string or a real number, not '${pythonTypeOf(val)}'`);
  });

  builtins["bool"] = callableVal("bool", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("bool() takes no keyword arguments");
    if (args.length === 0) return boolVal(false);
    if (args.length > 1) throw typeError(`bool expected at most 1 argument, got ${args.length}`);
    return boolVal(isTruthy(args[0]!));
  });

  builtins["str"] = callableVal("str", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("str() takes no keyword arguments");
    if (args.length === 0) return strVal("");
    if (args.length > 1) throw typeError(`str expected at most 1 argument, got ${args.length}`);
    const val = args[0]!;
    return strVal(strBudgeted(val, budget));
  });

  builtins["repr"] = callableVal("repr", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("repr() takes no keyword arguments");
    if (args.length !== 1) throw typeError(`repr() takes exactly one argument (${args.length} given)`);
    const val = args[0]!;
    return strVal(reprBudgeted(val, budget));
  });

  builtins["ascii"] = callableVal("ascii", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("ascii() takes no keyword arguments");
    if (args.length !== 1) throw typeError(`ascii() takes exactly one argument (${args.length} given)`);
    const val = args[0]!;
    return strVal(ascii(reprBudgeted(val, budget), budget));
  });

  builtins["len"] = callableVal("len", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("len() takes no keyword arguments");
    if (args.length !== 1) throw typeError(`len() takes exactly one argument (${args.length} given)`);
    const val = args[0]!;
    if (val.kind === "str") return intVal(BigInt(Array.from(val.value).length));
    if (val.kind === "bytes") return intVal(BigInt(val.value.length));
    if (val.kind === "list" || val.kind === "tuple") return intVal(BigInt(val.items.length));
    if (val.kind === "dict") return intVal(BigInt(val.entries.length));
    throw typeError(`object of type '${pythonTypeOf(val)}' has no len()`);
  });

  builtins["abs"] = callableVal("abs", (args, kwargs, budget) => {
    budget.tick();
    if (Object.keys(kwargs).length > 0) throw typeError("abs() takes no keyword arguments");
    if (args.length !== 1) throw typeError(`abs() takes exactly one argument (${args.length} given)`);
    const val = args[0]!;
    if (val.kind === "bool") return intVal(val.value ? 1n : 0n);
    if (val.kind === "int") return intVal(val.value < 0n ? checkI64(-val.value) : val.value);
    if (val.kind === "float") return floatVal(Math.abs(val.value));
    if (val.kind === "timedelta") return timeDeltaAbs(val);
    throw typeError(`bad operand type for abs(): '${pythonTypeOf(val)}'`);
  });

  builtins["round"] = callableVal("round", (args, kwargs, budget) => {
    budget.tick();
    const slots = bindSig({ name: "round", params: ["number", "ndigits"], required: 1, posOnly: 0, maxPos: 2 }, args, kwargs);
    const nd = slots[1];
    let ndigits: number | null = null;
    if (nd !== undefined && nd.kind !== "none") {
      if (nd.kind === "int") ndigits = Number(nd.value);
      else if (nd.kind === "bool") ndigits = nd.value ? 1 : 0;
      else throw typeError(`'${pythonTypeOf(nd)}' object cannot be interpreted as an integer`);
    }
    const val = slots[0]!;
    if (val.kind !== "int" && val.kind !== "float" && val.kind !== "bool") {
      throw typeError(`type ${pythonTypeOf(val)} doesn't define __round__ method`);
    }
    if (val.kind === "int") {
      return intVal(roundInt(val.value, ndigits));
    }
    if (val.kind === "bool") {
      const b = val.value ? 1n : 0n;
      return intVal(roundInt(b, ndigits));
    }
    const rounded = pyRoundExact(val.value, ndigits);
    if (ndigits === null) {
      if (Number.isNaN(rounded)) throw valueError("cannot convert float NaN to integer");
      if (!Number.isFinite(rounded)) throw overflowError("cannot convert float infinity to integer");
      return intVal(checkI64(BigInt(Math.trunc(rounded))));
    }
    return floatVal(rounded);
  });

  builtins["min"] = callableVal("min", (args, kwargs, budget) => {
    budget.tick();
    for (const key of Object.keys(kwargs)) {
      if (key !== "default") throw typeError(`min() got an unexpected keyword argument '${key}'`);
    }
    let items: Iterable<ExprValue>;
    const defaultVal = ownKwarg(kwargs, "default");
    const hasDefault = defaultVal !== undefined;

    if (args.length === 0) throw typeError("min expected at least 1 argument, got 0");
    if (args.length === 1) {
      items = iterateItems(args[0]!, budget);
    } else {
      if (hasDefault) throw typeError("Cannot specify a default for min() with multiple positional arguments");
      items = args;
    }

    let minVal: ExprValue | undefined;
    for (const cur of items) {
      if (minVal === undefined || evalCompare("lt", cur, minVal, budget)) minVal = cur;
    }
    if (minVal === undefined) {
      if (hasDefault && defaultVal) return defaultVal;
      throw valueError("min() iterable argument is empty");
    }

    return budget.admit(minVal);
  });

  builtins["max"] = callableVal("max", (args, kwargs, budget) => {
    budget.tick();
    for (const key of Object.keys(kwargs)) {
      if (key !== "default") throw typeError(`max() got an unexpected keyword argument '${key}'`);
    }
    let items: Iterable<ExprValue>;
    const defaultVal = ownKwarg(kwargs, "default");
    const hasDefault = defaultVal !== undefined;

    if (args.length === 0) throw typeError("max expected at least 1 argument, got 0");
    if (args.length === 1) {
      items = iterateItems(args[0]!, budget);
    } else {
      if (hasDefault) throw typeError("Cannot specify a default for max() with multiple positional arguments");
      items = args;
    }

    let maxVal: ExprValue | undefined;
    for (const cur of items) {
      if (maxVal === undefined || evalCompare("gt", cur, maxVal, budget)) maxVal = cur;
    }
    if (maxVal === undefined) {
      if (hasDefault && defaultVal) return defaultVal;
      throw valueError("max() iterable argument is empty");
    }

    return budget.admit(maxVal);
  });

  builtins["sum"] = callableVal("sum", (args, kwargs, budget) => {
    budget.tick();
    if (args.length === 0) throw typeError("sum expected at least 1 argument, got 0");
    if (args.length > 2) throw typeError(`sum() takes at most 2 arguments (${args.length} given)`);

    const slots = bindSig({ name: "sum", params: ["iterable", "start"], required: 1, posOnly: 1, maxPos: 2 }, args, kwargs);
    const startVal: ExprValue = slots[1] ?? intVal(0n);

    if (startVal.kind === "str") {
      throw typeError("sum() can't sum strings [use ''.join(seq) instead]");
    }

    if (startVal.kind === "bytes") throw typeError("sum() can't sum bytes [use b''.join(seq) instead]");
    let current: ExprValue = startVal;
    for (const item of iterateItems(args[0]!, budget)) {
      budget.tick();
      current = evalBinary("add", current, item, budget);
    }
    return current;
  });

  for (const name of ["list", "tuple"] as const) {
    builtins[name] = callableVal(name, (args, kwargs, budget) => {
      budget.tick();
      const slots = bindSig({ name, params: ["iterable"], required: 0, posOnly: 1, maxPos: 1 }, args, kwargs);
      const items = slots[0] === undefined ? [] : collectItems(slots[0], budget);
      budget.alloc(SLOT);
      return name === "list" ? listVal(items) : tupleVal(items);
    });
  }

  builtins["dict"] = callableVal("dict", (args, kwargs, budget) => {
    budget.tick();
    if (args.length > 1) throw typeError(`dict expected at most 1 argument, got ${args.length}`);
    const dictionary = new PyDict();
    const source = args[0];
    if (source?.kind === "dict") {
      const admitted = budget.admit(source);
      if (admitted.kind !== "dict") throw typeError("invalid expression dictionary copy");
      for (const [key, value] of admitted.entries) dictionary.set(key, value, budget);
    } else if (source !== undefined) {
      for (const item of collectItems(source, budget)) {
        if (item.kind !== "tuple" && item.kind !== "list") {
          throw typeError("cannot convert dictionary update sequence element to a sequence");
        }
        if (item.items.length !== 2) throw valueError(`dictionary update sequence element has length ${item.items.length}; 2 is required`);
        dictionary.set(item.items[0]!, item.items[1]!, budget);
      }
    }
    for (const key of Object.keys(kwargs)) {
      dictionary.set(strVal(key), budget.admit(kwargs[key]!), budget);
    }
    budget.checkValue({ kind: "dict", entries: dictionary.entries });
    budget.alloc(SLOT);
    return dictionary.toExprValue();
  });

  const dtClass = new DateTimeClass(clock);
  builtins["datetime"] = objectVal(dtClass);
  builtins["DateTime"] = objectVal(dtClass);
  builtins["time"] = objectVal(new TimeModule(clock));
  builtins["timedelta"] = callableVal("timedelta", (args, kwargs) => createTimeDelta(args, kwargs));
  builtins["relativedelta"] = callableVal("relativedelta", (args, kwargs) => createRelativeDelta(args, kwargs));
  builtins["Command"] = objectVal(new CommandNamespace());

  return builtins;
}

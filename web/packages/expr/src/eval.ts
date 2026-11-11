/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Budget, SLOT, computeDeepBytes, computeDepth, computeNodes, type EvalLimits, utf8ByteLength } from "./budget";
import { createStandardBuiltins } from "./builtins";
import { iterateItems } from "./iterable";
import { bindSig, sig0 } from "./args";
import {
  type Clock,
  callDateMethod,
  isDateMethod,
  timeDeltaTotalSeconds,
} from "./dates";
import type { ValidatedAst } from "./decoder";
import { assertHashable, keysEqual, PyDict } from "./dict";
import { CappedOut, strFormat, writeField } from "./format";
import {
  evalBinary,
  evalCompare,
  evalSubscript,
  evalUnary,
} from "./ops";
import { isPySpace, pyTitle } from "./pystr";
import {
  attributeError,
  nameError,
  PythonError,
  typeError,
  valueError,
} from "./python-error";
import {
  type CallableValue,
  type ExprValue,
  type ObjectValue,
  boolVal,
  bytesVal,
  callableVal,
  withCapturedValue,
  floatVal,
  intVal,
  isTruthy,
  listVal,
  newKwargs,
  NONE,
  ownKwarg,
  pythonTypeOf,
  repr,
  strVal,
  tupleVal,
} from "./values";

// Only bindings actually created for this context can receive builtin lookup semantics.
const contextBuiltins = new WeakMap<EvalContext, Readonly<Record<string, ExprValue>>>();

export interface EvalContext {
  readonly names: Record<string, ExprValue>;
  readonly limits: EvalLimits;
  readonly resolver?: (name: string, budget: Budget) => ExprValue | undefined;
}

export interface ContextOptions {
  readonly names?: Record<string, ExprValue>;
  readonly limits?: Partial<EvalLimits>;
  readonly clock?: Clock;
}

export function createContext(
  namesOrOptions: Record<string, ExprValue> | ContextOptions = {},
  limits?: Partial<EvalLimits>,
  clock?: Clock,
  resolver?: (name: string, budget: Budget) => ExprValue | undefined,
): EvalContext {
  let names: Record<string, ExprValue> = {};
  let effectiveLimits = limits;
  let effectiveClock = clock;

  if ("names" in namesOrOptions || "clock" in namesOrOptions || "limits" in namesOrOptions) {
    const opts = namesOrOptions as ContextOptions;
    names = opts.names ?? {};
    effectiveLimits = limits ?? opts.limits;
    effectiveClock = clock ?? opts.clock;
  } else {
    names = namesOrOptions as Record<string, ExprValue>;
  }

  const builtins = createStandardBuiltins(effectiveClock);
  // Copy descriptors, never invoke a host name getter while creating a context.
  const merged: Record<string, ExprValue> = Object.defineProperties(
    Object.assign(Object.create(null) as Record<string, ExprValue>, builtins), Object.getOwnPropertyDescriptors(names),
  );
  const budget = new Budget(effectiveLimits);
  const context: EvalContext = { names: merged, limits: budget.limits, resolver };
  contextBuiltins.set(context, builtins);
  return context;
}

function decodeFloatBits(hex: string): number {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  const bits = BigInt("0x" + hex);
  view.setBigUint64(0, bits);
  return view.getFloat64(0);
}

/** A `start`/`end` argument of `startswith`/`endswith`: an int, a bool, None or absent. */
function sliceBound(v: ExprValue | undefined): bigint | null {
  if (v === undefined || v.kind === "none") return null;
  if (v.kind === "int") return v.value;
  if (v.kind === "bool") return v.value ? 1n : 0n;
  throw typeError("slice indices must be integers or None or have an __index__ method");
}

/** Python's slice index normalisation for a sequence of `len` items: negatives count from the end. */
function normalizeBound(b: bigint | null, len: number, dflt: number, clampHigh: boolean): number {
  if (b === null) return dflt;
  const n = BigInt(len);
  let v = b;
  if (v < 0n) {
    v += n;
    if (v < 0n) v = 0n;
  } else if (clampHigh && v > n) {
    v = n;
  } else if (v > n + 1n) {
    // A start beyond the end only has to compare as "past the end".
    v = n + 1n;
  }
  return Number(v);
}

/** Integer argument as Python's `__index__` would accept it (int or bool). */
function indexArg(v: ExprValue): number {
  if (v.kind === "int") return Number(v.value);
  if (v.kind === "bool") return v.value ? 1 : 0;
  throw typeError(`'${pythonTypeOf(v)}' object cannot be interpreted as an integer`);
}

/** The items `str.join` iterates: list/tuple items, a str's characters, a dict's keys, bytes as ints. */
function joinItems(iter: ExprValue, budget: Budget): readonly ExprValue[] {
  switch (iter.kind) {
    case "list":
    case "tuple":
      return iter.items;
    case "str": {
      const chars = Array.from(iter.value);
      budget.steps(chars.length);
      return chars.map(strVal);
    }
    case "dict":
      budget.steps(iter.entries.length);
      return iter.entries.map(([k]) => k);
    case "bytes":
      budget.steps(iter.value.length);
      return Array.from(iter.value, (b) => intVal(BigInt(b)));
    default:
      throw typeError("can only join an iterable");
  }
}

/** Python `str.split()` without a separator: runs of Python whitespace separate. */
function splitWhitespace(s: string, maxsplit: number): string[] {
  const cps = Array.from(s);
  const n = cps.length;
  const parts: string[] = [];
  let i = 0;
  for (;;) {
    while (i < n && isPySpace(cps[i]!)) i += 1;
    if (i >= n) break;
    if (maxsplit >= 0 && parts.length >= maxsplit) {
      // The remainder keeps its trailing whitespace, as in Python.
      parts.push(cps.slice(i).join(""));
      break;
    }
    let j = i;
    while (j < n && !isPySpace(cps[j]!)) j += 1;
    parts.push(cps.slice(i, j).join(""));
    i = j;
  }
  return parts;
}

/** A bound `str` method, named `str.<attr>` in argument errors as CPython does. */
function bindStrMethod(s: string, attr: string): ExprValue | null {
  const method = buildStrMethod(s, attr);
  return method === null ? null : callableVal(method.name, method.call, `str.${attr}`);
}

function buildStrMethod(s: string, attr: string): CallableValue | null {
  if (attr === "startswith" || attr === "endswith") {
    return callableVal(attr, (args, kwargs, budget) => {
      budget.tick();
      if (Object.keys(kwargs).length > 0) throw typeError(`str.${attr}() takes no keyword arguments`);
      if (args.length === 0) throw typeError(`${attr}() takes at least 1 argument (0 given)`);
      if (args.length > 3) throw typeError(`${attr}() takes at most 3 arguments (${args.length} given)`);
      // CPython converts start and end before it looks at the prefix.
      const startArg = sliceBound(args[1]);
      const endArg = sliceBound(args[2]);
      const arg0 = args[0]!;
      let candidates: readonly ExprValue[];
      if (arg0.kind === "str") {
        candidates = [arg0];
      } else if (arg0.kind === "tuple") {
        candidates = arg0.items;
      } else {
        throw typeError(`${attr} first arg must be str or a tuple of str, not ${pythonTypeOf(arg0)}`);
      }
      budget.scan(utf8ByteLength(s));
      const cps = Array.from(s);
      const end = normalizeBound(endArg, cps.length, cps.length, true);
      const start = normalizeBound(startArg, cps.length, 0, false);
      // Tuple items are checked in order and the first match wins, so a later non-str item is
      // only an error when no earlier item matched (CPython).
      for (const item of candidates) {
        budget.tick();
        if (item.kind !== "str") {
          throw typeError(`tuple for ${attr} must only contain str, not ${pythonTypeOf(item)}`);
        }
        budget.scan(utf8ByteLength(item.value));
        const p = Array.from(item.value);
        if (end - start < p.length) continue;
        const window = attr === "startswith"
          ? cps.slice(start, start + p.length)
          : cps.slice(end - p.length, end);
        if (window.join("") === item.value) return boolVal(true);
      }
      return boolVal(false);
    });
  }

  if (attr === "split") {
    return callableVal("split", (args, kwargs, budget) => {
      budget.tick();
      const kwNames = Object.keys(kwargs);
      const total = args.length + kwNames.length;
      if (total > 2) {
        throw typeError(`split() takes at most 2 arguments (${total} given)`);
      }
      const params = ["sep", "maxsplit"];
      for (const key of kwNames) {
        const idx = params.indexOf(key);
        if (idx < 0) throw typeError(`'${key}' is an invalid keyword argument for split()`);
        if (idx < args.length) {
          throw typeError(`argument for split() given by name ('${key}') and position (${idx + 1})`);
        }
      }
      const sepVal = args[0] ?? ownKwarg(kwargs, "sep");
      const maxVal = args[1] ?? ownKwarg(kwargs, "maxsplit");
      let sep: string | null = null;
      if (sepVal !== undefined && sepVal.kind !== "none") {
        if (sepVal.kind !== "str") throw typeError(`must be str or None, not ${pythonTypeOf(sepVal)}`);
        sep = sepVal.value;
      }
      const maxsplit = maxVal === undefined ? -1 : indexArg(maxVal);
      budget.scan(utf8ByteLength(s));
      if (sep === "") {
        throw valueError("empty separator");
      }
      let parts: string[];
      if (sep !== null) {
        if (maxsplit < 0) {
          parts = s.split(sep);
        } else if (maxsplit === 0) {
          parts = [s];
        } else {
          const all = s.split(sep);
          parts = all.slice(0, maxsplit);
          if (all.length > maxsplit) parts.push(all.slice(maxsplit).join(sep));
        }
      } else {
        parts = splitWhitespace(s, maxsplit);
      }
      budget.alloc(32 * (parts.length + 1) + parts.reduce((acc, p) => acc + utf8ByteLength(p), 0));
      return listVal(parts.map(strVal));
    });
  }

  if (attr === "join") {
    return callableVal("join", (args, kwargs, budget) => {
      budget.tick();
      if (Object.keys(kwargs).length > 0) throw typeError("str.join() takes no keyword arguments");
      if (args.length !== 1) throw typeError(`str.join() takes exactly one argument (${args.length} given)`);
      const items = joinItems(args[0]!, budget);
      const pieces: string[] = [];
      let totalBytes = 0;
      for (let idx = 0; idx < items.length; idx++) {
        const item = items[idx]!;
        if (item.kind !== "str") {
          throw typeError(`sequence item ${idx}: expected str instance, ${pythonTypeOf(item)} found`);
        }
        pieces.push(item.value);
        totalBytes += utf8ByteLength(item.value);
      }
      const sepBytes = utf8ByteLength(s);
      const finalBytes = pieces.length > 0 ? totalBytes + sepBytes * (pieces.length - 1) : 0;
      budget.checkSize(32 + finalBytes);
      budget.charge(32 + finalBytes);
      return strVal(pieces.join(s));
    });
  }

  if (attr === "strip" || attr === "lstrip" || attr === "rstrip") {
    return callableVal(attr, (args, kwargs, budget) => {
      budget.tick();
      if (Object.keys(kwargs).length > 0) throw typeError(`str.${attr}() takes no keyword arguments`);
      if (args.length > 1) throw typeError(`${attr} expected at most 1 argument, got ${args.length}`);
      let charsSet: Set<string> | null = null;
      if (args.length === 1 && args[0]!.kind !== "none") {
        const cv = args[0]!;
        if (cv.kind !== "str") throw typeError(`${attr} arg must be None or str`);
        charsSet = new Set(Array.from(cv.value));
      }
      budget.scan(utf8ByteLength(s));
      const isMatch = (ch: string) => (charsSet ? charsSet.has(ch) : isPySpace(ch));
      const chars = Array.from(s);
      let start = 0;
      let end = chars.length;
      if (attr === "strip" || attr === "lstrip") {
        while (start < end && isMatch(chars[start]!)) {
          start += 1;
        }
      }
      if (attr === "strip" || attr === "rstrip") {
        while (end > start && isMatch(chars[end - 1]!)) {
          end -= 1;
        }
      }
      const outStr = chars.slice(start, end).join("");
      const outBytes = utf8ByteLength(outStr);
      budget.checkSize(32 + outBytes);
      budget.charge(32 + outBytes);
      return strVal(outStr);
    });
  }

  if (attr === "lower" || attr === "upper") {
    return callableVal(attr, (args, kwargs, budget) => {
      budget.tick();
      noArguments(`str.${attr}`, args, kwargs);
      budget.scan(utf8ByteLength(s));
      const outStr = attr === "lower" ? s.toLowerCase() : s.toUpperCase();
      const outBytes = utf8ByteLength(outStr);
      budget.checkSize(32 + outBytes);
      budget.charge(32 + outBytes);
      return strVal(outStr);
    });
  }

  if (attr === "capitalize") {
    return callableVal("capitalize", (args, kwargs, budget) => {
      budget.tick();
      noArguments("str.capitalize", args, kwargs);
      budget.scan(utf8ByteLength(s));
      const chars = Array.from(s);
      let outStr = "";
      if (chars.length > 0) {
        const head = chars[0]!;
        // Lowercase the whole string so a final sigma sees the character before it, then drop the
        // lowercase of the first character (a leading sigma is never final, so it is a prefix).
        const lowered = s.toLowerCase();
        const headLower = head.toLowerCase();
        if (!lowered.startsWith(headLower)) {
          throw new PythonError("SystemError", "capitalize: the lowercase of the first character is not a prefix of the lowercased string");
        }
        outStr = `${pyTitle(head)}${lowered.slice(headLower.length)}`;
      }
      const outBytes = utf8ByteLength(outStr);
      budget.checkSize(32 + outBytes);
      budget.charge(32 + outBytes);
      return strVal(outStr);
    });
  }

  if (attr === "replace") {
    return callableVal("replace", (args, kwargs, budget) => {
      budget.tick();
      if (Object.keys(kwargs).length > 0) throw typeError("str.replace() takes no keyword arguments");
      if (args.length < 2) throw typeError(`replace expected at least 2 arguments, got ${args.length}`);
      if (args.length > 3) throw typeError(`replace expected at most 3 arguments, got ${args.length}`);
      const oldVal = args[0]!;
      const newVal = args[1]!;
      if (oldVal.kind !== "str") throw typeError(`replace() argument 1 must be str, not ${pythonTypeOf(oldVal)}`);
      if (newVal.kind !== "str") throw typeError(`replace() argument 2 must be str, not ${pythonTypeOf(newVal)}`);
      const count = args.length === 3 ? indexArg(args[2]!) : -1;
      const oldStr = oldVal.value;
      const newStr = newVal.value;
      const sBytes = utf8ByteLength(s);
      const oldBytes = utf8ByteLength(oldStr);
      const newBytes = utf8ByteLength(newStr);
      budget.scan(sBytes);
      budget.scan(oldBytes);
      budget.scan(newBytes);
      // Find the matches first, predict the result size and charge it, and only then build the
      // string; the replacement is inserted literally (no JS `$` patterns).
      let outBytes: number;
      let build: () => string;
      if (oldStr === "") {
        const chars = Array.from(s);
        const n = count < 0 ? chars.length + 1 : Math.min(count, chars.length + 1);
        outBytes = sBytes + n * newBytes;
        build = () => {
          const pieces: string[] = [];
          for (let idx = 0; idx < chars.length; idx++) {
            if (idx < n) pieces.push(newStr);
            pieces.push(chars[idx]!);
          }
          if (chars.length < n) pieces.push(newStr);
          return pieces.join("");
        };
      } else {
        const positions: number[] = [];
        let at = count === 0 ? -1 : s.indexOf(oldStr);
        while (at >= 0) {
          positions.push(at);
          if (count >= 0 && positions.length >= count) break;
          at = s.indexOf(oldStr, at + oldStr.length);
        }
        budget.steps(positions.length);
        outBytes = sBytes + positions.length * (newBytes - oldBytes);
        build = () => {
          const pieces: string[] = [];
          let from = 0;
          for (const p of positions) {
            pieces.push(s.slice(from, p), newStr);
            from = p + oldStr.length;
          }
          pieces.push(s.slice(from));
          return pieces.join("");
        };
      }
      budget.checkSize(32 + outBytes);
      budget.charge(32 + outBytes);
      return strVal(build());
    });
  }

  if (attr === "format") {
    return callableVal("format", (args, kwargs, budget) => {
      budget.tick();
      return strVal(strFormat(s, args, kwargs, budget));
    });
  }

  return null;
}

/** CPython's check for a method that takes no arguments (`dict.keys`, `str.lower`). */
function noArguments(qualname: string, args: readonly ExprValue[], kwargs: Record<string, ExprValue>): void {
  if (Object.keys(kwargs).length > 0) throw typeError(`${qualname}() takes no keyword arguments`);
  if (args.length !== 0) throw typeError(`${qualname}() takes no arguments (${args.length} given)`);
}

/** The callable's name as CPython prints it before `()` in argument errors. */
function calleeName(fn: CallableValue | ObjectValue): string {
  // Host objects: their type name (CPython's text for host callables is not recorded).
  return fn.kind === "callable" ? (fn.qualname ?? fn.name) : fn.handle.typeName();
}

/** CPython's argument diagnostics use a callable name, or the repr of a noncallable callee. */
function argumentErrorTarget(fn: ExprValue): string {
  if (fn.kind === "callable") return `${calleeName(fn)}()`;
  if (fn.kind === "object" && fn.handle.call) return `${calleeName(fn)}()`;
  return repr(fn);
}

export function evalAst(ast: ValidatedAst, context: EvalContext): ExprValue {
  const budget = new Budget(context.limits);
  const nodes = ast.nodes;
  const rootIndex = nodes.length - 1;
  const internalCapabilities = new WeakSet<object>();
  const builtins = contextBuiltins.get(context);
  function owned<T extends ExprValue>(value: T, receiver?: ExprValue): T {
    if (value.kind === "callable" || value.kind === "object") {
      if (receiver !== undefined) withCapturedValue(value, receiver);
      budget.checkValue(value);
      internalCapabilities.add(value);
    }
    return value;
  }
  function isInternal(value: ExprValue): boolean { return internalCapabilities.has(value); }


  // Local scope stack for comprehensions and walrus bindings
  const scopes: Record<string, ExprValue>[] = [newKwargs()];

  function getVar(name: string): ExprValue {
    // Check scopes from top to bottom
    for (let i = scopes.length - 1; i >= 0; i--) {
      if (Object.hasOwn(scopes[i]!, name)) {
        return scopes[i]![name]!;
      }
    }
    // Explicit host names precede the resolver; standard builtins fall back after record fields.
    let builtin: ExprValue | undefined;
    if (Object.hasOwn(context.names, name)) {
      const descriptor = Object.getOwnPropertyDescriptor(context.names, name);
      if (!descriptor || !("value" in descriptor)) throw typeError("invalid expression value");
      const value = descriptor.value as ExprValue;
      if (builtins !== undefined && Object.hasOwn(builtins, name) && builtins[name] === value) builtin = value;
      else return budget.admit(value);
    }
    const resolved = context.resolver?.(name, budget);
    if (resolved !== undefined) return budget.admit(resolved);
    if (builtin !== undefined) return owned(builtin);
    throw nameError(name);
  }

  function setVar(name: string, val: ExprValue): void {
    scopes[scopes.length - 1]![name] = val;
  }

  function evalNode(idx: number): ExprValue {
    budget.enter();
    try {
      const node = nodes[idx]!;
      switch (node.kind) {
        case "literal": {
          budget.tick();
          const lit = node.value;
          switch (lit.kind) {
            case "none":
              return NONE;
            case "bool":
              return boolVal(lit.value);
            case "int":
              return intVal(lit.value);
            case "float":
              return floatVal(decodeFloatBits(lit.value));
            case "str":
              budget.alloc(32 + utf8ByteLength(lit.value));
              return strVal(lit.value);
            case "bytes":
              budget.alloc(32 + lit.value.length);
              return bytesVal(lit.value);
          }
        }
        case "name": {
          budget.tick();
          return getVar(node.name);
        }
        case "unary": {
          const operand = evalNode(node.operand);
          return evalUnary(node.op, operand, budget);
        }
        case "binary": {
          const left = evalNode(node.left);
          const right = evalNode(node.right);
          return evalBinary(node.op, left, right, budget);
        }
        case "bool_op": {
          budget.tick();
          if (node.op === "and") {
            let res: ExprValue = NONE;
            for (const childIdx of node.values) {
              res = evalNode(childIdx);
              if (!isTruthy(res)) return res;
            }
            return res;
          }
          if (node.op === "or") {
            let res: ExprValue = NONE;
            for (const childIdx of node.values) {
              res = evalNode(childIdx);
              if (isTruthy(res)) return res;
            }
            return res;
          }
          throw typeError(`unknown bool_op: ${node.op}`);
        }
        case "if_exp": {
          budget.tick();
          const test = evalNode(node.test);
          if (isTruthy(test)) {
            return evalNode(node.body);
          } else {
            return evalNode(node.orelse);
          }
        }
        case "compare": {
          budget.tick();
          let currentLeft = evalNode(node.left);
          for (let i = 0; i < node.ops.length; i++) {
            const nextRight = evalNode(node.comparators[i]!);
            if (!evalCompare(node.ops[i]!, currentLeft, nextRight, budget)) {
              return boolVal(false);
            }
            currentLeft = nextRight;
          }
          return boolVal(true);
        }
        case "attribute": {
          budget.tick();
          const target = evalNode(node.value);
          if (target.kind === "object") {
            const value = target.handle.getAttr(node.attr, budget);
            if (isInternal(target)) {
              budget.checkValue(value);
              return owned(value, target);
            }
            return budget.admit(value);
          }
          if (target.kind === "str") {
            const method = bindStrMethod(target.value, node.attr);
            if (method !== null) return owned(method, target);
          }
          if (target.kind === "dict") {
            const entries = target.entries;
            if (node.attr === "get") {
              return owned(callableVal("get", (args, kwargs, activeBudget) => {
                activeBudget.tick();
                if (Object.keys(kwargs).length > 0) throw typeError("dict.get() takes no keyword arguments");
                if (args.length === 0) throw typeError("get expected at least 1 argument, got 0");
                if (args.length > 2) throw typeError(`get expected at most 2 arguments, got ${args.length}`);
                const key = args[0]!;
                activeBudget.checkValue(key);
                assertHashable(key);
                for (const [candidate, value] of entries) {
                  activeBudget.steps(computeNodes(candidate) + computeNodes(key));
                  activeBudget.scan(computeDeepBytes(candidate) + computeDeepBytes(key));
                  if (keysEqual(candidate, key)) return activeBudget.admit(value);
                }
                return activeBudget.admit(args[1] ?? NONE);
              }, "dict.get"), target);
            }
            if (node.attr === "keys" || node.attr === "values" || node.attr === "items") {
              const method = node.attr;
              return owned(callableVal(method, (args, kwargs, activeBudget) => {
                activeBudget.tick();
                noArguments(`dict.${method}`, args, kwargs);
                let bytes = SLOT;
                let depth = 0;
                for (const [key, value] of entries) {
                  bytes += method === "keys" ? computeDeepBytes(key) : method === "values" ? computeDeepBytes(value)
                    : SLOT + computeDeepBytes(key) + computeDeepBytes(value);
                  depth = Math.max(depth, method === "keys" ? computeDepth(key) + 1 : method === "values" ? computeDepth(value) + 1
                    : Math.max(computeDepth(key), computeDepth(value)) + 2);
                  activeBudget.checkSize(bytes);
                  activeBudget.checkDepth(depth);
                }
                activeBudget.alloc(bytes);
                activeBudget.steps(entries.length);
                const items: ExprValue[] = [];
                for (const [key, value] of entries) items.push(method === "keys" ? key : method === "values" ? value : tupleVal([key, value]));
                return listVal(items);
              }, `dict.${method}`), target);
            }
          }
          if (target.kind === "date" || target.kind === "datetime") {
            if (node.attr === "year") return intVal(BigInt(target.year));
            if (node.attr === "month") return intVal(BigInt(target.month));
            if (node.attr === "day") return intVal(BigInt(target.day));
            if (target.kind === "datetime") {
              if (node.attr === "hour") return intVal(BigInt(target.hour));
              if (node.attr === "minute") return intVal(BigInt(target.minute));
              if (node.attr === "second") return intVal(BigInt(target.second));
              if (node.attr === "microsecond") return intVal(BigInt(target.microsecond));
            }
            if (isDateMethod(target, node.attr)) {
              const method = node.attr;
              return owned(callableVal(method, (args, kwargs, callBudget) =>
                callDateMethod(target, method, args, kwargs, callBudget),
              ), target);
            }
          }
          if (target.kind === "timedelta") {
            if (node.attr === "days") return intVal(BigInt(target.days));
            if (node.attr === "seconds") return intVal(BigInt(target.seconds));
            if (node.attr === "microseconds") return intVal(BigInt(target.microseconds));
            if (node.attr === "total_seconds") {
              return owned(callableVal("total_seconds", (args, kwargs, _budget) => {
                bindSig(sig0("total_seconds"), args, kwargs);
                return floatVal(timeDeltaTotalSeconds(target));
              }), target);
            }
          }
          if (target.kind === "relativedelta") {
            // Rust exposes the relative and absolute integer fields; `weekday` is not an attribute there.
            switch (node.attr) {
              case "years":
              case "months":
              case "days":
              case "leapdays":
              case "hours":
              case "minutes":
              case "seconds":
              case "microseconds":
                return intVal(target[node.attr]);
              case "year":
              case "month":
              case "day":
              case "hour":
              case "minute":
              case "second":
              case "microsecond": {
                const val = target[node.attr];
                return val !== null ? intVal(val) : NONE;
              }
            }
          }
          throw attributeError(pythonTypeOf(target), node.attr);
        }
        case "subscript": {
          const target = evalNode(node.value);
          if (node.slice.kind === "index") {
            const sliceVal = evalNode(node.slice.value);
            return evalSubscript(target, { kind: "index", value: sliceVal }, budget);
          }
          const lower = node.slice.lower !== null ? evalNode(node.slice.lower) : null;
          const upper = node.slice.upper !== null ? evalNode(node.slice.upper) : null;
          const step = node.slice.step !== null ? evalNode(node.slice.step) : null;
          return evalSubscript(target, { kind: "slice", lower, upper, step }, budget);
        }
        case "call": {
          budget.tick();
          const fn = evalNode(node.func);

          const positionalArgs: ExprValue[] = [];
          for (const argIdx of node.args) {
            const argNode = nodes[argIdx]!;
            if (argNode.kind === "starred") {
              const unpacked = evalNode(argNode.value);
              if (unpacked.kind !== "str" && unpacked.kind !== "bytes" && unpacked.kind !== "list" &&
                unpacked.kind !== "tuple" && unpacked.kind !== "dict") {
                throw typeError(`${argumentErrorTarget(fn)} argument after * must be an iterable, not ${pythonTypeOf(unpacked)}`);
              }
              for (const item of iterateItems(unpacked, budget)) {
                positionalArgs.push(item);
              }
            } else {
              const value = evalNode(argIdx);
              positionalArgs.push(value);
            }
          }

          // No prototype: `__proto__`, `constructor` and the like are ordinary keyword names.
          const keywordArgs = newKwargs();
          const namedKeywords = new Set<string>();
          for (const kw of node.keywords) {
            if (kw.name === null) {
              const unpacked = evalNode(kw.value);
              if (unpacked.kind !== "dict") {
                throw typeError(`${argumentErrorTarget(fn)} argument after ** must be a mapping, not ${pythonTypeOf(unpacked)}`);
              }
              for (const [k, v] of unpacked.entries) {
                if (k.kind !== "str") {
                  throw typeError("keywords must be strings");
                }
                if (Object.hasOwn(keywordArgs, k.value)) {
                  throw typeError(`${argumentErrorTarget(fn)} got multiple values for keyword argument '${k.value}'`);
                }
                keywordArgs[k.value] = v;
              }
            } else {
              const value = evalNode(kw.value);
              if (namedKeywords.has(kw.name)) {
                // `f(a=1, a=2)`: the Rust parser rejects it; a hand-built AST gets the same error.
                throw new PythonError("SyntaxError", `keyword argument repeated: ${kw.name}`);
              }
              if (Object.hasOwn(keywordArgs, kw.name)) {
                throw typeError(`${argumentErrorTarget(fn)} got multiple values for keyword argument '${kw.name}'`);
              }
              namedKeywords.add(kw.name);
              keywordArgs[kw.name] = value;
            }
          }

          if (fn.kind !== "callable" && fn.kind !== "object") {
            throw typeError(`'${pythonTypeOf(fn)}' object is not callable`);
          }

          if (fn.kind === "callable") {
            const value = fn.call(positionalArgs, keywordArgs, budget);
            if (!isInternal(fn)) return budget.admit(value);
            budget.checkValue(value);
            return value;
          }
          if (fn.handle.call) {
            const value = fn.handle.call(positionalArgs, keywordArgs, budget);
            if (!isInternal(fn)) return budget.admit(value);
            budget.checkValue(value);
            return value;
          }
          throw typeError(`'${fn.handle.typeName()}' object is not callable`);
        }
        case "list": {
          budget.alloc(32 * (node.items.length + 1));
          const items: ExprValue[] = [];
          for (const itemIdx of node.items) {
            items.push(evalNode(itemIdx));
          }
          return listVal(items);
        }
        case "tuple": {
          budget.alloc(32 * (node.items.length + 1));
          const items: ExprValue[] = [];
          for (const itemIdx of node.items) {
            items.push(evalNode(itemIdx));
          }
          return tupleVal(items);
        }
        case "dict": {
          budget.alloc(32 + node.items.length * 80);
          const dict = new PyDict();
          for (const item of node.items) {
            const k = evalNode(item[0]!);
            const v = evalNode(item[1]!);
            dict.set(k, v);
          }
          return dict.toExprValue();
        }
        case "walrus": {
          budget.tick();
          const val = evalNode(node.value);
          setVar(node.name, val);
          return val;
        }
        case "list_comp":
        case "generator_exp": {
          budget.tick();
          const collected: ExprValue[] = [];
          scopes.push(newKwargs());
          try {
            evalComprehensionLevel(node.generators, 0, node.elt, collected);
          } finally {
            scopes.pop();
          }
          budget.alloc(32 * (collected.length + 1));
          return listVal(collected);
        }
        case "joined_str": {
          budget.tick();
          const out = new CappedOut(budget);
          for (const itemIdx of node.items) {
            const itemNode = nodes[itemIdx]!;
            if (itemNode.kind === "literal" && itemNode.value.kind === "str") {
              out.pushStr(itemNode.value.value, budget);
            } else if (itemNode.kind === "formatted_value") {
              const v = evalNode(itemNode.value);
              const spec = itemNode.format_spec ?? "";
              writeField(out, v, itemNode.conversion, spec, budget);
            } else {
              const v = evalNode(itemIdx);
              writeField(out, v, null, "", budget);
            }
          }
          return strVal(out.finish(budget));
        }
        case "formatted_value": {
          budget.tick();
          const out = new CappedOut(budget);
          const v = evalNode(node.value);
          const spec = node.format_spec ?? "";
          writeField(out, v, node.conversion, spec, budget);
          return strVal(out.finish(budget));
        }
        case "starred":
          throw typeError("starred expression outside call/unpacking");
      }
    } finally {
      budget.leave();
    }
  }

  function unpackTarget(targetIdx: number, val: ExprValue): void {
    const targetNode = nodes[targetIdx]!;
    if (targetNode.kind === "name") {
      setVar(targetNode.name, val);
      return;
    }
    if (targetNode.kind === "tuple" || targetNode.kind === "list") {
      let items: readonly ExprValue[];
      if (val.kind === "list" || val.kind === "tuple") {
        items = val.items;
      } else {
        throw typeError(`cannot unpack non-iterable ${pythonTypeOf(val)} object`);
      }
      if (items.length < targetNode.items.length) {
        throw valueError(`not enough values to unpack (expected ${targetNode.items.length}, got ${items.length})`);
      }
      if (items.length > targetNode.items.length) {
        throw valueError(`too many values to unpack (expected ${targetNode.items.length})`);
      }
      for (let i = 0; i < targetNode.items.length; i++) {
        unpackTarget(targetNode.items[i]!, items[i]!);
      }
      return;
    }
    throw typeError("invalid comprehension target");
  }

  function evalComprehensionLevel(
    generators: readonly { target: number; iter: number; ifs: readonly number[] }[],
    level: number,
    eltIdx: number,
    collected: ExprValue[],
  ): void {
    if (level >= generators.length) {
      budget.tick();
      budget.alloc(32);
      const val = evalNode(eltIdx);
      collected.push(val);
      return;
    }

    const gen = generators[level]!;
    const iterVal = evalNode(gen.iter);
    let items: readonly ExprValue[];

    if (iterVal.kind === "list" || iterVal.kind === "tuple") {
      items = iterVal.items;
    } else if (iterVal.kind === "str") {
      items = Array.from(iterVal.value).map(strVal);
    } else if (iterVal.kind === "bytes") {
      items = Array.from(iterVal.value).map(b => intVal(BigInt(b)));
    } else if (iterVal.kind === "dict") {
      items = iterVal.entries.map(([k]) => k);
    } else {
      throw typeError(`'${pythonTypeOf(iterVal)}' object is not iterable`);
    }

    for (const item of items) {
      unpackTarget(gen.target, item);
      let passed = true;
      for (const ifIdx of gen.ifs) {
        budget.tick();
        const test = evalNode(ifIdx);
        if (!isTruthy(test)) {
          passed = false;
          break;
        }
      }
      if (passed) {
        evalComprehensionLevel(generators, level + 1, eltIdx, collected);
      }
    }
  }

  return evalNode(rootIndex);
}

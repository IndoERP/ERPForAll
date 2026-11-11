/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  type Budget,
  computeNodes,
  utf8ByteLength,
} from "./budget";
import {
  indexError,
  keyError,
  limitExceeded,
  overflowError,
  PythonError,
  typeError,
  valueError,
} from "./python-error";
import { dateIso, dateTimeIso, strftime } from "./dates";
import {
  exponentForm,
  fixedDigits,
  generalForm,
  predictedDigits,
} from "./float-digits";
import {
  type ExprValue,
  floatRepr,
  ownKwarg,
  pythonTypeOf,
  repr,
  strVal,
} from "./values";

export interface Spec {
  fill: string | null;
  align: "<" | ">" | "=" | "^" | null;
  sign: "+" | "-" | " " | null;
  noNegZero: boolean;
  alt: boolean;
  zero: boolean;
  width: number;
  grouping: "," | "_" | null;
  precision: number | null;
  ty: string | null;
}

export class CappedOut {
  private buf = "";
  private bytes = 0;
  private readonly cap: number;
  private readonly reason: string;

  constructor(budget: Budget) {
    const totalLeft = Math.max(0, budget.limits.maxTotalBytes - budget.bytesUsed);
    if (totalLeft < budget.limits.maxValueBytes) {
      this.cap = totalLeft;
      this.reason = `evaluation memory budget of ${budget.limits.maxTotalBytes} bytes exceeded`;
    } else {
      this.cap = budget.limits.maxValueBytes;
      this.reason = `string exceeds the per-value limit of ${budget.limits.maxValueBytes} bytes`;
    }
  }

  get length(): number {
    return this.buf.length;
  }

  push(ch: string, budget: Budget): void {
    const b = utf8ByteLength(ch);
    if (this.bytes + b > this.cap) {
      budget.fail(limitExceeded(this.reason));
    }
    this.buf += ch;
    this.bytes += b;
  }

  pushStr(s: string, budget: Budget): void {
    const b = utf8ByteLength(s);
    if (this.bytes + b > this.cap) {
      budget.fail(limitExceeded(this.reason));
    }
    this.buf += s;
    this.bytes += b;
  }

  pad(fill: string, count: number, budget: Budget): void {
    if (count <= 0) return;
    const str = fill.repeat(count);
    this.pushStr(str, budget);
  }

  finish(budget: Budget): string {
    budget.scan(this.bytes);
    budget.alloc(this.bytes);
    return this.buf;
  }
}

function parseNumber(chars: readonly string[], pos: { i: number }, budget: Budget): number | null {
  const start = pos.i;
  let n = 0;
  while (pos.i < chars.length && /^[0-9]$/.test(chars[pos.i]!)) {
    n = n * 10 + parseInt(chars[pos.i]!, 10);
    pos.i += 1;
  }
  if (pos.i === start) {
    return null;
  }
  budget.checkFormatWidth(n);
  return n;
}

export function parseSpec(spec: string, v: ExprValue, budget: Budget): Spec {
  const chars = Array.from(spec);
  const s: Spec = {
    fill: null,
    align: null,
    sign: null,
    noNegZero: false,
    alt: false,
    zero: false,
    width: 0,
    grouping: null,
    precision: null,
    ty: null,
  };
  const pos = { i: 0 };
  const isAlign = (c: string | undefined): boolean => c === "<" || c === ">" || c === "=" || c === "^";

  if (chars.length > 1 && isAlign(chars[1])) {
    s.fill = chars[0]!;
    s.align = chars[1] as "<" | ">" | "=" | "^";
    pos.i = 2;
  } else if (chars.length > 0 && isAlign(chars[0])) {
    s.align = chars[0] as "<" | ">" | "=" | "^";
    pos.i = 1;
  }

  if (chars[pos.i] === "+" || chars[pos.i] === "-" || chars[pos.i] === " ") {
    s.sign = chars[pos.i] as "+" | "-" | " ";
    pos.i += 1;
  }
  if (chars[pos.i] === "z") {
    s.noNegZero = true;
    pos.i += 1;
  }
  if (chars[pos.i] === "#") {
    s.alt = true;
    pos.i += 1;
  }
  if (chars[pos.i] === "0") {
    s.zero = true;
    pos.i += 1;
  }
  const w = parseNumber(chars, pos, budget);
  if (w !== null) {
    s.width = w;
  }
  if (chars[pos.i] === "," || chars[pos.i] === "_") {
    s.grouping = chars[pos.i] as "," | "_";
    pos.i += 1;
  }
  if (chars[pos.i] === ".") {
    pos.i += 1;
    const p = parseNumber(chars, pos, budget);
    if (p === null) {
      throw valueError("Format specifier missing precision");
    }
    s.precision = p;
  }
  // One remaining character is the presentation type; each formatter rejects the ones it lacks
  // with "Unknown format code".
  if (pos.i === chars.length - 1) {
    s.ty = chars[pos.i]!;
    pos.i += 1;
  }
  if (pos.i !== chars.length) {
    throw valueError(`Invalid format specifier '${spec}' for object of type '${pythonTypeOf(v)}'`);
  }
  return s;
}

function pad(
  out: CappedOut,
  body: string,
  prefix: string,
  s: Spec,
  defaultAlign: "<" | ">" | "=" | "^",
  budget: Budget,
): void {
  const len = Array.from(body).length + Array.from(prefix).length;
  const fill = s.fill ?? (s.zero ? "0" : " ");
  const align = s.align ?? (s.zero && defaultAlign === ">" ? "=" : defaultAlign);
  const padding = Math.max(0, s.width - len);
  switch (align) {
    case "<":
      out.pushStr(prefix, budget);
      out.pushStr(body, budget);
      out.pad(fill, padding, budget);
      break;
    case "^": {
      const left = Math.floor(padding / 2);
      const right = padding - left;
      out.pad(fill, left, budget);
      out.pushStr(prefix, budget);
      out.pushStr(body, budget);
      out.pad(fill, right, budget);
      break;
    }
    case "=":
      out.pushStr(prefix, budget);
      out.pad(fill, padding, budget);
      out.pushStr(body, budget);
      break;
    case ">":
    default:
      out.pad(fill, padding, budget);
      out.pushStr(prefix, budget);
      out.pushStr(body, budget);
      break;
  }
}

function groupDigits(digits: string, sep: string, every: number): string {
  const n = digits.length;
  let out = "";
  for (let i = 0; i < n; i++) {
    if (i > 0 && (n - i) % every === 0) {
      out += sep;
    }
    out += digits[i]!;
  }
  return out;
}

/**
 * CPython groups the zero padding itself (`'{:010,}'.format(1234)` is `00,001,234`). The client
 * does not implement that; it refuses instead of printing ungrouped zeros (recorded divergence,
 * ).
 */
function refuseGroupedZeroPadding(s: Spec, body: string, prefix: string): void {
  if (s.grouping === null) return;
  // Effective fill and alignment exactly as `pad` computes them for a number (default '>'):
  // `{:010,}`, `{:0=10,}` and `{:=010,}` all pad with '0' after the sign.
  const fill = s.fill ?? (s.zero ? "0" : " ");
  const align = s.align ?? (s.zero ? "=" : ">");
  if (fill !== "0" || align !== "=") return;
  if (s.width > Array.from(body).length + Array.from(prefix).length) {
    throw new PythonError(
      "NotImplementedError",
      "zero padding combined with a grouping option is not supported by the client evaluator",
    );
  }
}

function signPrefix(negative: boolean, s: Spec): string {
  if (negative) return "-";
  if (s.sign === "+") return "+";
  if (s.sign === " ") return " ";
  return "";
}

function formatStr(text: string, s: Spec, out: CappedOut, budget: Budget): void {
  if (s.sign !== null) {
    throw valueError("Sign not allowed in string format specifier");
  }
  if (s.noNegZero) {
    throw valueError("Negative zero coercion (z) not allowed in string format specifier");
  }
  if (s.alt) {
    throw valueError("Alternate form (#) not allowed in string format specifier");
  }
  if (s.align === "=") {
    throw valueError("'=' alignment not allowed in string format specifier");
  }
  if (s.ty !== null && s.ty !== "s") {
    throw valueError(`Unknown format code '${s.ty}' for object of type 'str'`);
  }
  const body = s.precision !== null ? Array.from(text).slice(0, s.precision).join("") : text;
  pad(out, body, "", s, "<", budget);
}

function formatInt(i: bigint, s: Spec, out: CappedOut, v: ExprValue, budget: Budget): void {
  if (s.precision !== null) {
    throw valueError("Precision not allowed in integer format specifier");
  }
  if (s.noNegZero) {
    throw valueError("Negative zero coercion (z) not allowed in integer format specifier");
  }
  if (s.ty === "c") {
    if (s.sign !== null) {
      throw valueError("Sign not allowed with integer format specifier 'c'");
    }
    if (s.alt) {
      throw valueError("Alternate form (#) not allowed with integer format specifier 'c'");
    }
  }
  const negative = i < 0n;
  const mag = negative ? -i : i;
  let digits = "";
  let altPrefix = "";
  let groupEvery = 3;

  switch (s.ty) {
    case null:
    case "d":
    case "n":
      digits = mag.toString();
      break;
    case "b":
      digits = mag.toString(2);
      altPrefix = "0b";
      groupEvery = 4;
      break;
    case "o":
      digits = mag.toString(8);
      altPrefix = "0o";
      groupEvery = 4;
      break;
    case "x":
      digits = mag.toString(16).toLowerCase();
      altPrefix = "0x";
      groupEvery = 4;
      break;
    case "X":
      digits = mag.toString(16).toUpperCase();
      altPrefix = "0X";
      groupEvery = 4;
      break;
    case "c": {
      if (i < 0n || i >= 0x110000n) {
        throw overflowError("%c arg not in range(0x110000)");
      }
      const c = String.fromCodePoint(Number(i));
      // `c` is a numeric presentation: right-aligned by default, like the other integer types.
      pad(out, c, "", s, ">", budget);
      return;
    }
    default:
      throw valueError(`Unknown format code '${s.ty}' for object of type '${pythonTypeOf(v)}'`);
  }

  const body = s.grouping ? groupDigits(digits, s.grouping, groupEvery) : digits;
  let prefix = signPrefix(negative, s);
  if (s.alt) {
    prefix += altPrefix;
  }
  refuseGroupedZeroPadding(s, body, prefix);
  pad(out, body, prefix, s, ">", budget);
}

/** Python `float(int)`: rounds to nearest, and an int beyond the float range is an OverflowError. */
export function intToFloat(i: bigint): number {
  const f = Number(i);
  if (!Number.isFinite(f)) {
    throw overflowError("int too large to convert to float");
  }
  return f;
}

/** Whether a formatted float body shows only zero digits (for the `z` option). */
function allZeroDigits(body: string): boolean {
  const idx = body.search(/[eE%]/);
  const mantissa = idx >= 0 ? body.slice(0, idx) : body;
  return /^[0.]+$/.test(mantissa);
}

function formatFloat(x: number, s: Spec, out: CappedOut, budget: Budget): void {
  const upper = s.ty === "E" || s.ty === "F" || s.ty === "G";
  let negative = (Object.is(x, -0) || x < 0) && !Number.isNaN(x);
  // Python multiplies in float arithmetic for `%` (so 1e307 becomes inf), then formats with `f`.
  const mag = s.ty === "%" ? Math.abs(x) * 100 : Math.abs(x);
  let body: string;

  if (!Number.isFinite(mag)) {
    const t = Number.isNaN(mag) ? "nan" : "inf";
    body = upper ? t.toUpperCase() : t;
    if (s.ty === "%") body += "%";
  } else {
    // Digit generation is charged before it runs; precision is already bounded by checkFormatWidth.
    const precision = s.precision ?? 6;
    budget.scan(predictedDigits(mag, precision));
    switch (s.ty) {
      case "f":
      case "F":
        body = fixedDigits(mag, precision, s.alt);
        break;
      case "e":
      case "E":
        body = exponentForm(mag, precision, upper, s.alt);
        break;
      case "g":
      case "G":
      case "n":
        body = generalForm(mag, precision, upper, s.alt, false);
        break;
      case "%":
        body = `${fixedDigits(mag, precision, s.alt)}%`;
        break;
      case null:
        if (s.precision !== null) {
          body = generalForm(mag, s.precision, false, s.alt, true);
        } else {
          body = floatRepr(mag);
          if (s.alt && !body.includes(".")) {
            const idx = body.search(/[eE]/);
            body = `${body.slice(0, idx)}.${body.slice(idx)}`;
          }
        }
        break;
      default:
        throw valueError(`Unknown format code '${s.ty}' for object of type 'float'`);
    }
    // `z` turns a negative zero, also one produced by rounding, into a positive zero.
    if (s.noNegZero && negative && allZeroDigits(body)) {
      negative = false;
    }
  }

  if (s.grouping && Number.isFinite(mag)) {
    const match = body.match(/^([^.eE%]+)(.*)$/);
    if (match) {
      const intPart = match[1]!;
      const rest = match[2]!;
      body = `${groupDigits(intPart, s.grouping, 3)}${rest}`;
    }
  }

  // CPython pads inf and nan with plain zeros even with a grouping option (`0000000inf`).
  if (Number.isFinite(mag)) refuseGroupedZeroPadding(s, body, signPrefix(negative, s));
  pad(out, body, signPrefix(negative, s), s, ">", budget);
}

export function formatValue(v: ExprValue, spec: string, budget: Budget): string {
  if (spec.length === 0) {
    return strBudgeted(v, budget);
  }
  if (v.kind === "date") {
    return strftime(v, { hour: 0, minute: 0, second: 0, microsecond: 0 }, spec, budget);
  }
  if (v.kind === "datetime") {
    return strftime(v, v, spec, budget);
  }
  const s = parseSpec(spec, v, budget);
  checkGrouping(s, v);
  const out = new CappedOut(budget);
  switch (v.kind) {
    case "str":
      formatStr(v.value, s, out, budget);
      break;
    case "int":
    case "bool": {
      const i = v.kind === "int" ? v.value : v.value ? 1n : 0n;
      if (s.ty !== null && ["e", "E", "f", "F", "g", "G", "%"].includes(s.ty)) {
        formatFloat(intToFloat(i), s, out, budget);
      } else {
        formatInt(i, s, out, v, budget);
      }
      break;
    }
    case "float":
      if (s.ty !== null && ["d", "b", "o", "x", "X", "c", "s"].includes(s.ty)) {
        throw valueError(`Unknown format code '${s.ty}' for object of type 'float'`);
      }
      formatFloat(v.value, s, out, budget);
      break;
    default:
      throw typeError(`unsupported format string passed to ${pythonTypeOf(v)}.__format__`);
  }
  return out.finish(budget);
}

/**
 * CPython rejects a grouping option the presentation type cannot take while parsing the spec,
 * before any type-specific check: `,` only with d/e/f/g/%/no type, `_` also with b/o/x/X.
 */
function checkGrouping(s: Spec, v: ExprValue): void {
  if (s.grouping === null) return;
  let ty: string | null;
  if (v.kind === "str") ty = s.ty ?? "s";
  else if (v.kind === "int" || v.kind === "bool") ty = s.ty ?? "d";
  else if (v.kind === "float") ty = s.ty;
  else return;
  if (ty === null || "deEfFgG%".includes(ty)) return;
  if (s.grouping === "_" && "boxX".includes(ty)) return;
  throw valueError(`Cannot specify '${s.grouping}' with '${ty}'.`);
}

export function writeStr(out: CappedOut, val: ExprValue, budget: Budget): void {
  switch (val.kind) {
    case "str":
      out.pushStr(val.value, budget);
      break;
    case "date":
      out.pushStr(dateIso(val), budget);
      break;
    case "datetime":
      out.pushStr(dateTimeIso(val, " "), budget);
      break;
    case "timedelta": {
      // Preserve DATES' normalized string spelling in FORMAT's bounded output sink.
      if (val.days !== 0) {
        out.pushStr(`${val.days} day${Math.abs(val.days) === 1 ? "" : "s"}, `, budget);
      }
      const hours = Math.floor(val.seconds / 3600);
      const minutes = Math.floor((val.seconds % 3600) / 60);
      const seconds = val.seconds % 60;
      out.pushStr(`${hours}:${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`, budget);
      if (val.microseconds !== 0) out.pushStr(`.${String(val.microseconds).padStart(6, "0")}`, budget);
      break;
    }
    default:
      writeRepr(out, val, budget);
      break;
  }
}

export function writeRepr(out: CappedOut, val: ExprValue, budget: Budget): void {
  out.pushStr(repr(val), budget);
}

export function strBudgeted(val: ExprValue, budget: Budget): string {
  budget.steps(computeNodes(val));
  const out = new CappedOut(budget);
  writeStr(out, val, budget);
  return out.finish(budget);
}

export function reprBudgeted(val: ExprValue, budget: Budget): string {
  budget.steps(computeNodes(val));
  const out = new CappedOut(budget);
  writeRepr(out, val, budget);
  return out.finish(budget);
}

export function ascii(r: string, budget: Budget): string {
  const out = new CappedOut(budget);
  for (const c of r) {
    const cp = c.codePointAt(0)!;
    if (cp <= 127) {
      out.push(c, budget);
    } else if (cp <= 0xff) {
      out.pushStr(`\\x${cp.toString(16).padStart(2, "0")}`, budget);
    } else if (cp <= 0xffff) {
      out.pushStr(`\\u${cp.toString(16).padStart(4, "0")}`, budget);
    } else {
      out.pushStr(`\\U${cp.toString(16).padStart(8, "0")}`, budget);
    }
  }
  return out.finish(budget);
}

export function convert(v: ExprValue, conv: string | null, budget: Budget): ExprValue {
  switch (conv) {
    case null:
      return v;
    case "s":
      return strVal(strBudgeted(v, budget));
    case "r":
      return strVal(reprBudgeted(v, budget));
    case "a":
      return strVal(ascii(reprBudgeted(v, budget), budget));
    default:
      throw valueError(`Unknown conversion specifier ${conv}`);
  }
}

export function writeField(
  out: CappedOut,
  value: ExprValue,
  conv: string | null,
  spec: string,
  budget: Budget,
): void {
  if (spec.length === 0) {
    if (conv === null || conv === "s") {
      budget.steps(computeNodes(value));
      writeStr(out, value, budget);
      return;
    }
    if (conv === "r") {
      budget.steps(computeNodes(value));
      writeRepr(out, value, budget);
      return;
    }
  }
  let piece: string;
  if (conv === null) {
    piece = formatValue(value, spec, budget);
  } else {
    const converted = convert(value, conv, budget);
    piece = formatValue(converted, spec, budget);
  }
  out.pushStr(piece, budget);
}

export function strFormat(
  template: string,
  args: readonly ExprValue[],
  kwargs: Record<string, ExprValue>,
  budget: Budget,
): string {
  budget.scan(template.length);
  const out = new CappedOut(budget);
  const chars = Array.from(template);
  let auto: number | null = null;
  let manual = false;
  let i = 0;

  while (i < chars.length) {
    const c = chars[i]!;
    if (c === "{" && i + 1 < chars.length && chars[i + 1] === "{") {
      out.push("{", budget);
      i += 2;
      continue;
    }
    if (c === "}" && i + 1 < chars.length && chars[i + 1] === "}") {
      out.push("}", budget);
      i += 2;
      continue;
    }
    if (c === "}") {
      throw valueError("Single '}' encountered in format string");
    }
    if (c === "{") {
      let field = "";
      let closed = false;
      i += 1;
      while (i < chars.length) {
        const f = chars[i]!;
        if (f === "}") {
          closed = true;
          i += 1;
          break;
        }
        if (f === "{") {
          throw valueError("nested replacement fields in format specs are not supported");
        }
        field += f;
        i += 1;
      }
      if (!closed) {
        throw valueError("Single '{' encountered in format string");
      }

      const colonIdx = field.indexOf(":");
      const head = colonIdx >= 0 ? field.slice(0, colonIdx) : field;
      const spec = colonIdx >= 0 ? field.slice(colonIdx + 1) : "";

      const exclIdx = head.indexOf("!");
      const name = exclIdx >= 0 ? head.slice(0, exclIdx) : head;
      let conv: string | null = null;
      if (exclIdx >= 0) {
        const cStr = head.slice(exclIdx + 1);
        if (cStr.length !== 1) {
          throw valueError("expected ':' after conversion specifier");
        }
        conv = cStr;
      }

      if (name.includes(".") || name.includes("[")) {
        throw valueError("format field accessors ('.' and '[') are not supported");
      }

      let value: ExprValue;
      if (name.length === 0) {
        if (manual) {
          throw valueError("cannot switch from manual field specification to automatic field numbering");
        }
        const idx: number = auto === null ? 0 : auto + 1;
        auto = idx;
        if (idx >= args.length) {
          throw indexError(`Replacement index ${idx} out of range for positional args tuple`);
        }
        value = args[idx]!;
      } else if (/^[0-9]+$/.test(name)) {
        if (auto !== null) {
          throw valueError("cannot switch from automatic field numbering to manual field specification");
        }
        manual = true;
        const idx = parseInt(name, 10);
        if (idx >= args.length) {
          throw indexError(`Replacement index ${idx} out of range for positional args tuple`);
        }
        value = args[idx]!;
      } else {
        const kv = ownKwarg(kwargs, name);
        if (kv === undefined) {
          throw keyError(repr(strVal(name)));
        }
        value = kv;
      }

      writeField(out, value, conv, spec, budget);
    } else {
      out.push(c, budget);
      i += 1;
    }
  }

  return out.finish(budget);
}

export function printf(template: string, arg: ExprValue, budget: Budget): string {
  budget.scan(template.length);
  let args: readonly ExprValue[];
  let mapping: ExprValue | null = null;

  if (arg.kind === "tuple") {
    args = arg.items;
  } else if (arg.kind === "dict") {
    args = [arg];
    mapping = arg;
  } else {
    args = [arg];
  }

  let nextArg = 0;
  let usedMapping = false;
  const out = new CappedOut(budget);
  const chars = Array.from(template);
  let i = 0;

  while (i < chars.length) {
    const c = chars[i]!;
    i += 1;
    if (c !== "%") {
      out.push(c, budget);
      continue;
    }

    budget.tick();
    let value: ExprValue | null = null;

    if (chars[i] === "(") {
      if (mapping === null || mapping.kind !== "dict") {
        throw typeError("format requires a mapping");
      }
      let depth = 1;
      const keyStart = i + 1;
      let j = keyStart;
      while (j < chars.length) {
        const k = chars[j]!;
        if (k === "(") depth += 1;
        else if (k === ")") {
          depth -= 1;
          if (depth === 0) break;
        }
        j += 1;
      }
      if (depth !== 0) {
        throw valueError("incomplete format key");
      }
      const key = chars.slice(keyStart, j).join("");
      let found: ExprValue | null = null;
      for (const [k, v] of mapping.entries) {
        if (k.kind === "str" && k.value === key) {
          found = v;
          break;
        }
      }
      if (found === null) {
        throw keyError(repr(strVal(key)));
      }
      value = found;
      usedMapping = true;
      i = j + 1;
    }

    const flags: Spec = {
      fill: null,
      align: null,
      sign: null,
      noNegZero: false,
      alt: false,
      zero: false,
      width: 0,
      grouping: null,
      precision: null,
      ty: null,
    };
    let left = false;

    while (i < chars.length) {
      const f = chars[i]!;
      if (f === "-") left = true;
      else if (f === "+") flags.sign = "+";
      else if (f === " ") {
        if (flags.sign === null) flags.sign = " ";
      } else if (f === "#") flags.alt = true;
      else if (f === "0") flags.zero = true;
      else break;
      i += 1;
    }

    const pos = { i };
    if (chars[pos.i] === "*") {
      pos.i += 1;
      if (nextArg >= args.length) {
        throw typeError("not enough arguments for format string");
      }
      const wVal = args[nextArg++]!;
      if (wVal.kind !== "int" && wVal.kind !== "bool") {
        throw typeError("* wants int");
      }
      let w = Number(wVal.kind === "int" ? wVal.value : wVal.value ? 1 : 0);
      if (w < 0) {
        left = true;
        w = -w;
      }
      budget.checkFormatWidth(w);
      flags.width = w;
    } else {
      const w = parseNumber(chars, pos, budget);
      if (w !== null) {
        flags.width = w;
      }
    }

    if (chars[pos.i] === ".") {
      pos.i += 1;
      if (chars[pos.i] === "*") {
        pos.i += 1;
        if (nextArg >= args.length) {
          throw typeError("not enough arguments for format string");
        }
        const pVal = args[nextArg++]!;
        if (pVal.kind !== "int" && pVal.kind !== "bool") {
          throw typeError("* wants int");
        }
        let p = Number(pVal.kind === "int" ? pVal.value : pVal.value ? 1 : 0);
        p = Math.max(0, p);
        budget.checkFormatWidth(p);
        flags.precision = p;
      } else {
        const p = parseNumber(chars, pos, budget);
        flags.precision = p ?? 0;
      }
    }
    i = pos.i;

    while (i < chars.length && (chars[i] === "h" || chars[i] === "l" || chars[i] === "L")) {
      i += 1;
    }

    if (i >= chars.length) {
      throw valueError("incomplete format");
    }
    const conv = chars[i]!;
    i += 1;

    if (conv === "%") {
      out.push("%", budget);
      continue;
    }

    if (left) {
      flags.align = "<";
      flags.zero = false;
    }

    let v: ExprValue;
    if (value !== null) {
      v = value;
    } else {
      if (mapping !== null && arg.kind !== "tuple") {
        usedMapping = true;
      }
      if (nextArg >= args.length) {
        throw typeError("not enough arguments for format string");
      }
      v = args[nextArg++]!;
    }

    printfOne(conv, i - 1, v, flags, out, budget);
  }

  if (nextArg < args.length && !usedMapping && mapping === null) {
    throw typeError("not all arguments converted during string formatting");
  }

  return out.finish(budget);
}

function printfOne(
  conv: string,
  index: number,
  v: ExprValue,
  flags: Spec,
  out: CappedOut,
  budget: Budget,
): void {
  switch (conv) {
    case "d":
    case "i":
    case "u": {
      let i: bigint;
      if (v.kind === "float") {
        if (Number.isNaN(v.value)) {
          throw valueError("cannot convert float NaN to integer");
        }
        if (!Number.isFinite(v.value)) {
          throw overflowError("cannot convert float infinity to integer");
        }
        i = BigInt(Math.trunc(v.value));
      } else if (v.kind === "int") {
        i = v.value;
      } else if (v.kind === "bool") {
        i = v.value ? 1n : 0n;
      } else {
        throw typeError(`%${conv} format: a real number is required, not ${pythonTypeOf(v)}`);
      }
      printfInt(i, 10, false, flags, out, budget);
      break;
    }
    case "o":
    case "x":
    case "X": {
      let i: bigint;
      if (v.kind === "int") {
        i = v.value;
      } else if (v.kind === "bool") {
        i = v.value ? 1n : 0n;
      } else {
        throw typeError(`%${conv} format: an integer is required, not ${pythonTypeOf(v)}`);
      }
      printfInt(i, conv === "o" ? 8 : 16, conv === "X", flags, out, budget);
      break;
    }
    case "e":
    case "E":
    case "f":
    case "F":
    case "g":
    case "G": {
      let f: number;
      if (v.kind === "float") f = v.value;
      else if (v.kind === "int") f = intToFloat(v.value);
      else if (v.kind === "bool") f = v.value ? 1 : 0;
      else throw typeError(`must be real number, not ${pythonTypeOf(v)}`);

      const spec: Spec = {
        ...flags,
        ty: conv,
        precision: flags.precision ?? 6,
      };
      if (spec.zero && spec.align === null) {
        spec.align = "=";
        spec.fill = "0";
      }
      formatFloat(f, spec, out, budget);
      break;
    }
    case "c": {
      let c: string;
      if (v.kind === "str" && Array.from(v.value).length === 1) {
        c = v.value;
      } else if (v.kind === "int" || v.kind === "bool") {
        const i = v.kind === "int" ? v.value : v.value ? 1n : 0n;
        if (i < 0n || i >= 0x110000n) {
          throw overflowError("%c arg not in range(0x110000)");
        }
        c = String.fromCodePoint(Number(i));
      } else {
        throw typeError("%c requires int or char");
      }
      pad(out, c, "", { ...flags, zero: false }, ">", budget);
      break;
    }
    case "s":
    case "r":
      if (flags.width === 0 && flags.precision === null) {
        budget.steps(computeNodes(v));
        if (conv === "s") {
          writeStr(out, v, budget);
        } else {
          writeRepr(out, v, budget);
        }
      } else {
        const text = conv === "s" ? strBudgeted(v, budget) : reprBudgeted(v, budget);
        const body = flags.precision !== null ? Array.from(text).slice(0, flags.precision).join("") : text;
        pad(out, body, "", { ...flags, zero: false }, ">", budget);
      }
      break;
    case "a": {
      const text = ascii(reprBudgeted(v, budget), budget);
      const body = flags.precision !== null ? Array.from(text).slice(0, flags.precision).join("") : text;
      pad(out, body, "", { ...flags, zero: false }, ">", budget);
      break;
    }
    default:
      throw valueError(
        `unsupported format character '${conv}' (0x${conv.charCodeAt(0).toString(16)}) at index ${index}`,
      );
  }
}

function printfInt(
  i: bigint,
  radix: number,
  upper: boolean,
  flags: Spec,
  out: CappedOut,
  budget: Budget,
): void {
  const negative = i < 0n;
  const mag = negative ? -i : i;
  let digits: string;
  if (radix === 8) {
    digits = mag.toString(8);
  } else if (radix === 16) {
    digits = upper ? mag.toString(16).toUpperCase() : mag.toString(16).toLowerCase();
  } else {
    digits = mag.toString(10);
  }

  if (flags.precision !== null && digits.length < flags.precision) {
    digits = "0".repeat(flags.precision - digits.length) + digits;
  }

  let prefix = signPrefix(negative, flags);
  if (flags.alt) {
    prefix += radix === 8 ? "0o" : radix === 16 ? (upper ? "0X" : "0x") : "";
  }

  const spec: Spec = {
    fill: flags.zero && flags.align === null ? "0" : " ",
    align: flags.align ?? (flags.zero ? "=" : ">"),
    sign: null,
    noNegZero: false,
    alt: false,
    zero: false,
    width: flags.width,
    grouping: null,
    precision: null,
    ty: null,
  };

  pad(out, digits, prefix, spec, ">", budget);
}

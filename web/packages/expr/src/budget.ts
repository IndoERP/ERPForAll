/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { limitExceeded, overflowError, typeError, valueError, type PythonError } from "./python-error";
import { capabilityCaptures, isRegisteredCapability, type ExprValue } from "./values";

export const SLOT = 32;
export const RELATIVE_DELTA_PAYLOAD = 144;
export const DICT_ENTRY_OVERHEAD = 16;
export const BYTES_PER_STEP = 64;

export interface EvalLimits {
  readonly maxSourceBytes: number;
  readonly maxDepth: number;
  readonly maxSteps: number;
  readonly maxValueBytes: number;
  readonly maxTotalBytes: number;
  readonly maxFormatWidth: number;
}

export const EVAL_LIMITS_CEILING: EvalLimits = Object.freeze({
  maxSourceBytes: 64 * 1024,
  maxDepth: 100,
  maxSteps: 1_000_000,
  maxValueBytes: 1024 * 1024,
  maxTotalBytes: 16 * 1024 * 1024,
  maxFormatWidth: 1_000,
});

/** UTF-8 size without allocating an encoded copy. Unpaired surrogates encode as replacement characters. */
export function utf8ByteLength(s: string): number {
  let bytes = 0;
  for (let i = 0; i < s.length; i++) {
    const unit = s.charCodeAt(i);
    if (unit < 0x80) bytes++;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < s.length &&
      s.charCodeAt(i + 1) >= 0xdc00 && s.charCodeAt(i + 1) <= 0xdfff) { bytes += 4; i++; }
    else bytes += 3;
  }
  return bytes;
}

/** Read data properties only: admission must never call a host data getter. */
function ownData(value: object, key: PropertyKey): unknown {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  if (!descriptor || !("value" in descriptor)) throw typeError("invalid expression value");
  return descriptor.value;
}
const typedArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype) as object;
const byteLengthGetter = Object.getOwnPropertyDescriptor(typedArrayPrototype, "byteLength")!.get!;
function bytesLength(value: unknown): number {
  if (!(value instanceof Uint8Array)) throw typeError("invalid expression value");
  return byteLengthGetter.call(value) as number;
}
const DATE_FIELDS = ["year", "month", "day"] as const;
const TIME_FIELDS = ["hour", "minute", "second", "microsecond"] as const;
const DELTA_FIELDS = ["days", "seconds", "microseconds"] as const;
const REL_FIELDS = ["years", "months", "days", "leapdays", "hours", "minutes", "seconds", "microseconds"] as const;
const ABS_FIELDS = ["year", "month", "day", "hour", "minute", "second", "microsecond"] as const;
function integer(value: unknown, min: number, max: number): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min || value > max) {
    throw typeError("invalid expression value");
  }
  return value;
}
function i64(value: unknown): void {
  if (typeof value !== "bigint") throw typeError("invalid expression value");
  if (value < -9223372036854775808n || value > 9223372036854775807n) throw overflowError("integer overflow");
}
interface ValueMeta { readonly bytes: number; readonly nodes: number; readonly depth: number; }

/** One bounded metadata pass. No payload/container copy occurs before all checks succeed. */
function inspectValue(value: unknown, budget: Budget, admitting: boolean): ValueMeta {
  const active = new WeakSet<object>();
  let bytes = 0, nodes = 0, height = 0;
  function capacity(extraNodes: number, extraBytes: number): void {
    const size = bytes + extraBytes;
    budget.checkSize(size);
    if (admitting) budget.checkAvailable(size);
    const steps = nodes + extraNodes;
    if (steps > budget.limits.maxSteps - (admitting ? budget.stepsUsed : 0)) {
      budget.fail(limitExceeded(`evaluation step limit of ${budget.limits.maxSteps} exceeded`));
    }
  }
  function visit(raw: unknown, depth: number): void {
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) throw typeError("invalid expression value");
    if (active.has(raw)) throw valueError("cyclic expression value");
    budget.checkDepth(depth);
    capacity(1, SLOT); nodes++; bytes += SLOT; height = Math.max(height, depth);
    const kind = ownData(raw, "kind");
    switch (kind) {
      case "none": return;
      case "bool": if (typeof ownData(raw, "value") !== "boolean") throw typeError("invalid expression value"); return;
      case "int": i64(ownData(raw, "value")); return;
      case "float": if (typeof ownData(raw, "value") !== "number") throw typeError("invalid expression value"); return;
      case "str": {
        const text = ownData(raw, "value");
        if (typeof text !== "string") throw typeError("invalid expression value");
        capacity(0, text.length); // cheap lower bound before a UTF-8 scan
        const length = utf8ByteLength(text); capacity(0, length); bytes += length; return;
      }
      case "bytes": { const length = bytesLength(ownData(raw, "value")); capacity(0, length); bytes += length; return; }
      case "callable": case "object": {
        if (!isRegisteredCapability(raw)) throw typeError("invalid expression value");
        active.add(raw);
        for (const captured of capabilityCaptures(raw)) visit(captured, depth + 1);
        active.delete(raw);
        return;
      }
      case "date": case "datetime": {
        const year = integer(ownData(raw, "year"), 1, 9999);
        const month = integer(ownData(raw, "month"), 1, 12);
        const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
        const days = month === 2 ? (leap ? 29 : 28) : [4, 6, 9, 11].includes(month) ? 30 : 31;
        integer(ownData(raw, "day"), 1, days);
        if (kind === "datetime") {
          for (const field of TIME_FIELDS) integer(ownData(raw, field), 0,
            field === "hour" ? 23 : field === "microsecond" ? 999999 : 59);
        }
        return;
      }
      case "timedelta":
        integer(ownData(raw, "days"), -999999999, 999999999);
        integer(ownData(raw, "seconds"), 0, 86399); integer(ownData(raw, "microseconds"), 0, 999999); return;
      case "relativedelta": {
        capacity(0, RELATIVE_DELTA_PAYLOAD); bytes += RELATIVE_DELTA_PAYLOAD;
        for (const field of REL_FIELDS) i64(ownData(raw, field));
        for (const field of ABS_FIELDS) { const n = ownData(raw, field); if (n !== null) i64(n); }
        const weekday = ownData(raw, "weekday");
        if (weekday !== null) {
          if (typeof weekday !== "object") throw typeError("invalid expression value");
          integer(ownData(weekday, "day"), 0, 6);
          const n = ownData(weekday, "n"); if (n !== null) integer(n, Number.MIN_SAFE_INTEGER, Number.MAX_SAFE_INTEGER);
        }
        return;
      }
      case "list": case "tuple": case "dict": {
        const items = ownData(raw, kind === "dict" ? "entries" : "items");
        if (!Array.isArray(items)) throw typeError("invalid expression value");
        const length = ownData(items, "length") as number;
        capacity(length * (kind === "dict" ? 2 : 1), length * (kind === "dict" ? SLOT * 2 + DICT_ENTRY_OVERHEAD : SLOT));
        active.add(raw);
        for (let i = 0; i < length; i++) {
          const item = ownData(items, i);
          if (kind === "dict") {
            if (!Array.isArray(item) || ownData(item, "length") !== 2) throw typeError("invalid expression value");
            bytes += DICT_ENTRY_OVERHEAD;
            visit(ownData(item, 0), depth + 1); visit(ownData(item, 1), depth + 1);
          } else visit(item, depth + 1);
        }
        active.delete(raw); return;
      }
      default: throw typeError("invalid expression value");
    }
  }
  visit(value, 0);
  return { bytes, nodes, depth: height };
}

/** Copy only after bounded validation and charging; capabilities retain their registered identity. */
function snapshot(value: ExprValue): ExprValue {
  const kind = ownData(value, "kind") as ExprValue["kind"];
  if (kind === "object" || kind === "callable") return value;
  const result: Record<string, unknown> = { kind };
  if (kind === "list" || kind === "tuple") {
    const source = ownData(value, "items") as readonly ExprValue[];
    const items: ExprValue[] = [];
    for (let i = 0; i < source.length; i++) items.push(snapshot(ownData(source, i) as ExprValue));
    result["items"] = Object.freeze(items);
  } else if (kind === "dict") {
    const source = ownData(value, "entries") as readonly (readonly [ExprValue, ExprValue])[];
    const entries: (readonly [ExprValue, ExprValue])[] = [];
    for (let i = 0; i < source.length; i++) {
      const pair = ownData(source, i) as readonly [ExprValue, ExprValue];
      entries.push(Object.freeze([snapshot(ownData(pair, 0) as ExprValue), snapshot(ownData(pair, 1) as ExprValue)] as const));
    }
    result["entries"] = Object.freeze(entries);
  } else if (kind === "bytes") {
    const bytes = ownData(value, "value") as Uint8Array;
    const copy = new Uint8Array(bytesLength(bytes));
    Uint8Array.prototype.set.call(copy, bytes); result["value"] = copy;
  } else if (kind === "bool" || kind === "int" || kind === "float" || kind === "str") {
    result["value"] = ownData(value, "value");
  } else {
    const fields = kind === "date" ? DATE_FIELDS : kind === "datetime" ? [...DATE_FIELDS, ...TIME_FIELDS]
      : kind === "timedelta" ? DELTA_FIELDS : kind === "relativedelta" ? [...REL_FIELDS, ...ABS_FIELDS, "weekday"] : [];
    for (const field of fields) {
      const v = ownData(value, field);
      result[field] = field === "weekday" && v !== null
        ? Object.freeze({ day: ownData(v as object, "day"), n: ownData(v as object, "n") }) : v;
    }
  }
  return Object.freeze(result) as unknown as ExprValue;
}

/** Bounded logical metadata helpers; malformed or over-ceiling values are refused. */
export function computeDeepBytes(val: ExprValue): number { return inspectValue(val, new Budget(), false).bytes; }
export function computeNodes(val: ExprValue): number { return inspectValue(val, new Budget(), false).nodes; }
export function computeDepth(val: ExprValue): number { return inspectValue(val, new Budget(), false).depth; }

export class Budget {
  readonly limits: EvalLimits;
  private _steps = 0;
  private _bytes = 0;
  private _depth = 0;
  private _stickyError: PythonError | null = null;

  constructor(limits: Partial<EvalLimits> = {}) {
    if (typeof limits !== "object" || limits === null ||
      (Object.getPrototypeOf(limits) !== Object.prototype && Object.getPrototypeOf(limits) !== null)) {
      throw valueError("evaluation limits must be a plain data object");
    }
    const requested: Partial<Record<keyof EvalLimits, number>> = Object.create(null) as Partial<Record<keyof EvalLimits, number>>;
    for (const key of Reflect.ownKeys(limits)) {
      if (typeof key !== "string" || !Object.hasOwn(EVAL_LIMITS_CEILING, key)) throw valueError(`unknown evaluation limit '${String(key)}'`);
      const descriptor = Object.getOwnPropertyDescriptor(limits, key);
      if (!descriptor || !("value" in descriptor)) throw valueError(`evaluation limit '${key}' must be a data property`);
      const value: unknown = descriptor.value;
      if (value !== undefined) {
        if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
          throw valueError(`evaluation limit '${key}' must be a positive safe integer`);
        }
        requested[key as keyof EvalLimits] = value;
      }
    }
    this.limits = Object.freeze({
      maxSourceBytes: Math.min(requested.maxSourceBytes ?? EVAL_LIMITS_CEILING.maxSourceBytes, EVAL_LIMITS_CEILING.maxSourceBytes),
      maxDepth: Math.min(requested.maxDepth ?? EVAL_LIMITS_CEILING.maxDepth, EVAL_LIMITS_CEILING.maxDepth),
      maxSteps: Math.min(requested.maxSteps ?? EVAL_LIMITS_CEILING.maxSteps, EVAL_LIMITS_CEILING.maxSteps),
      maxValueBytes: Math.min(requested.maxValueBytes ?? EVAL_LIMITS_CEILING.maxValueBytes, EVAL_LIMITS_CEILING.maxValueBytes),
      maxTotalBytes: Math.min(requested.maxTotalBytes ?? EVAL_LIMITS_CEILING.maxTotalBytes, EVAL_LIMITS_CEILING.maxTotalBytes),
      maxFormatWidth: Math.min(requested.maxFormatWidth ?? EVAL_LIMITS_CEILING.maxFormatWidth, EVAL_LIMITS_CEILING.maxFormatWidth),
    });
  }

  get stepsUsed(): number {
    return this._steps;
  }

  get bytesUsed(): number {
    return this._bytes;
  }

  get depth(): number {
    return this._depth;
  }

  get stickyError(): PythonError | null {
    return this._stickyError;
  }

  private checkSticky(): void {
    if (this._stickyError) {
      throw this._stickyError;
    }
  }

  fail(err: PythonError): never {
    if (!this._stickyError) {
      this._stickyError = err;
    }
    throw err;
  }

  tick(): void {
    this.steps(1);
  }

  steps(n: number): void {
    this.checkSticky();
    this._steps += n;
    if (this._steps > this.limits.maxSteps) {
      this.fail(limitExceeded(`evaluation step limit of ${this.limits.maxSteps} exceeded`));
    }
  }

  scan(byteCount: number): void {
    this.steps(Math.floor(byteCount / BYTES_PER_STEP) + 1);
  }

  checkSize(bytes: number): void {
    this.checkSticky();
    if (bytes > this.limits.maxValueBytes) {
      this.fail(limitExceeded(`value of ${bytes} bytes exceeds the per-value limit of ${this.limits.maxValueBytes} bytes`));
    }
  }

  checkDepth(depth: number): void {
    this.checkSticky();
    if (depth > this.limits.maxDepth) {
      this.fail(limitExceeded(`value nesting depth limit of ${this.limits.maxDepth} exceeded`));
    }
  }

  charge(bytes: number): void {
    this.checkSticky();
    const total = this._bytes + bytes;
    if (total > this.limits.maxTotalBytes) {
      this.fail(limitExceeded(`evaluation memory budget of ${this.limits.maxTotalBytes} bytes exceeded`));
    }
    this._bytes = total;
  }

  alloc(bytes: number): void {
    this.checkSize(bytes);
    this.charge(bytes);
  }

  /** Check a planned copy against remaining memory before allocating scratch or payloads. */
  checkAvailable(bytes: number): void {
    this.checkSticky();
    if (bytes > this.limits.maxTotalBytes - this._bytes) {
      this.fail(limitExceeded(`evaluation memory budget of ${this.limits.maxTotalBytes} bytes exceeded`));
    }
  }

  /** Validate, charge once, then snapshot an externally produced value. */
  admit(val: ExprValue): ExprValue {
    this.checkSticky();
    const meta = inspectValue(val, this, true);
    this.alloc(meta.bytes);
    this.steps(meta.nodes);
    return snapshot(val);
  }

  checkValue(val: ExprValue): void {
    this.checkSticky();
    inspectValue(val, this, false);
  }

  enter(): void {
    this.checkSticky();
    if (this._depth >= this.limits.maxDepth) {
      this.fail(limitExceeded(`nesting depth limit of ${this.limits.maxDepth} exceeded`));
    }
    this._depth += 1;
  }

  leave(): void {
    this._depth = Math.max(0, this._depth - 1);
  }

  /**
   * A capped output buffer for a string of unknown final length (Rust `Budget::out`): the cap is the
   * smaller of the per-value limit and what is left of the total budget, with the matching message.
   */
  out(): OutBuffer {
    const totalLeft = Math.max(0, this.limits.maxTotalBytes - this._bytes);
    if (totalLeft < this.limits.maxValueBytes) {
      return new OutBuffer(this, totalLeft, `evaluation memory budget of ${this.limits.maxTotalBytes} bytes exceeded`);
    }
    return new OutBuffer(
      this,
      this.limits.maxValueBytes,
      `string exceeds the per-value limit of ${this.limits.maxValueBytes} bytes`,
    );
  }

  /** Charges a finished output buffer (Rust `Budget::finish`: scan, then alloc of its length). */
  finish(out: OutBuffer): string {
    const s = out.toString();
    this.scan(out.byteLength);
    this.alloc(out.byteLength);
    return s;
  }

  checkFormatWidth(w: number): void {
    this.checkSticky();
    if (w > this.limits.maxFormatWidth) {
      this.fail(limitExceeded(`format width or precision ${w} exceeds the limit of ${this.limits.maxFormatWidth}`));
    }
  }
}

/** Output buffer of `Budget.out()`; a push past the cap is a `LimitExceeded` (Rust `Out`). */
export class OutBuffer {
  private readonly parts: string[] = [];
  private bytes = 0;

  constructor(
    private readonly budget: Budget,
    private readonly cap: number,
    private readonly reason: string,
  ) {}

  get byteLength(): number {
    return this.bytes;
  }

  push(s: string): void {
    const n = utf8ByteLength(s);
    if (this.bytes + n > this.cap) {
      this.budget.fail(limitExceeded(this.reason));
    }
    this.parts.push(s);
    this.bytes += n;
  }

  toString(): string {
    return this.parts.join("");
  }
}

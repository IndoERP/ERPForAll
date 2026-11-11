/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { fail, fieldPath, lowerLimits, type AllocationCounter } from "./errors";

export type JsonValue = null | boolean | number | string | readonly JsonValue[] | { readonly [key: string]: JsonValue };
export const RESPONSE_LIMITS = Object.freeze({ maxBytes: 64 * 1024 * 1024, maxDepth: 64 as number, maxAllocationBytes: 64 * 1024 * 1024 });
export type JsonNumberPolicy = "finite" | "exact_decimal";
export type ReaderLimits = Partial<typeof RESPONSE_LIMITS>;
const NO_LIMITS: ReaderLimits = Object.freeze({});
type Location = { readonly parent: Location | undefined; readonly key: string | number };
interface Stats { bytes: number; tokens: number; logicalAllocationBytes: number; applicationAllocations: number; }
interface Context { readonly stats: Stats; }
interface Metadata { readonly context: Context; readonly start: number; readonly end: number; readonly depth: number; readonly location: Location | undefined; }
const origins = new WeakMap<object, Metadata>();
const UTF8_DECODER = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const ASCII_TEXT = /^[\x00-\x7f]*$/;
const typedArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype) as object;
const typedArrayTag = Object.getOwnPropertyDescriptor(typedArrayPrototype, Symbol.toStringTag)!.get!;
const typedArrayByteLength = Object.getOwnPropertyDescriptor(typedArrayPrototype, "byteLength")!.get!;
function pathOf(location: Location | undefined, counter?: AllocationCounter): string {
  if (counter) counter.sites++;
  const segments: (string | number)[] = [];
  for (let cursor = location; cursor; cursor = cursor.parent) segments.push(cursor.key);
  let path = "$";
  for (let i = segments.length - 1; i >= 0; i--) {
    const segment = segments[i]!;
    if (typeof segment === "number") { if (counter) counter.sites++; path = `${path}[${segment}]`; }
    else path = fieldPath(path, segment, counter);
  }
  return path;
}
/** Internal metadata lookup authenticates without inspecting an object's properties or prototype. */
export function parsedMetadata(value: unknown): Metadata | undefined {
  return typeof value === "object" && value !== null ? origins.get(value) : undefined;
}
/** Diagnostic counters count application sites, never total JS-engine allocations. */
export function inspectParsed(value: unknown) {
  const meta = parsedMetadata(value);
  if (!meta) fail("$", "value must originate from the bounded JSON reader");
  return Object.freeze({ ...meta.context.stats, encodedBytes: meta.end - meta.start, depth: meta.depth, path: pathOf(meta.location) });
}
export function parsedPath(meta: Metadata, counter?: AllocationCounter): string { return pathOf(meta.location, counter); }

function utf8Length(text: string): number {
  if (ASCII_TEXT.test(text)) return text.length;
  let bytes = 0;
  for (let i = 0; i < text.length; i++) {
    const unit = text.charCodeAt(i);
    if (unit < 0x80) bytes++;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length && text.charCodeAt(i + 1) >= 0xdc00 && text.charCodeAt(i + 1) <= 0xdfff) { bytes += 4; i++; }
    else if (unit >= 0xd800 && unit <= 0xdfff) fail("$", "unpaired Unicode surrogate");
    else bytes += 3;
  }
  return bytes;
}

/** Normalize decimal spelling without expanding exponents or rounding through Number. */
function decimalIdentity(source: string): string {
  const match = /^(-?)([0-9]+)(?:\.([0-9]+))?(?:[eE]([+-]?[0-9]+))?$/.exec(source);
  if (!match) fail("$", "invalid decimal token");
  const fraction = match[3] ?? "";
  const coefficient = (match[2]! + fraction).replace(/^0+/, "");
  if (!coefficient) return "0";
  const digits = coefficient.replace(/0+$/, "");
  const exponent = Number(match[4] ?? "0") - fraction.length + coefficient.length - digits.length;
  return `${match[1]}${digits}e${exponent}`;
}
function numberPolicy(value: JsonNumberPolicy): void {
  if (value !== "finite" && value !== "exact_decimal") fail("$.numberPolicy", "unknown JSON number policy");
}

class Reader {
  private cursor = 0;
  private byteCursor = 0;
  private height = 0;
  private readonly path: (string | number)[] = [];
  private readonly context: Context;
  private readonly number = /-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/y;
  constructor(private readonly text: string, private readonly limits: typeof RESPONSE_LIMITS, bytes: number, inputAllocations: number, private readonly numberPolicy: JsonNumberPolicy) {
    this.context = { stats: { bytes, tokens: 0, logicalAllocationBytes: 0, applicationAllocations: 7 + inputAllocations } };
  }
  private error(_location: Location | undefined, detail: string): never {
    let path = "$";
    for (const key of this.path) path = typeof key === "number" ? `${path}[${key}]` : fieldPath(path, key);
    return fail(path, detail);
  }
  private reserve(bytes: number, location: Location | undefined): void {
    if (bytes > this.limits.maxAllocationBytes - this.context.stats.logicalAllocationBytes) this.error(location, `logical allocation limit exceeded (${this.limits.maxAllocationBytes})`);
    this.context.stats.logicalAllocationBytes += bytes;
  }
  private ascii(count = 1): void { this.cursor += count; this.byteCursor += count; }
  private whitespace(): void {
    while (true) {
      const char = this.text[this.cursor];
      if (char !== " " && char !== "\r" && char !== "\n" && char !== "\t") return;
      this.ascii();
    }
  }
  private unit(location: Location | undefined): number {
    if (this.cursor >= this.text.length) this.error(location, "unterminated JSON string");
    const unit = this.text.charCodeAt(this.cursor);
    if (unit < 32) this.error(location, "unescaped control character");
    if (unit === 34) this.error(location, "unpaired Unicode surrogate");
    if (unit !== 92) {
      this.cursor++;
      // Paired raw surrogates are four UTF-8 bytes; the following low unit adds zero.
      this.byteCursor += unit < 128 ? 1 : unit < 2048 ? 2 : unit >= 0xd800 && unit <= 0xdbff ? 4 : unit >= 0xdc00 && unit <= 0xdfff ? 0 : 3;
      return unit;
    }
    this.ascii();
    const escape = this.text[this.cursor]; this.ascii();
    switch (escape) {
      case '"': return 34; case "\\": return 92; case "/": return 47;
      case "b": return 8; case "f": return 12; case "n": return 10; case "r": return 13; case "t": return 9;
      case "u": {
        let value = 0;
        for (let index = 0; index < 4; index++) {
          const digit = this.text.charCodeAt(this.cursor + index);
          let nibble: number;
          if (digit >= 48 && digit <= 57) nibble = digit - 48;
          else if (digit >= 65 && digit <= 70) nibble = digit - 55;
          else if (digit >= 97 && digit <= 102) nibble = digit - 87;
          else return this.error(location, "invalid Unicode escape");
          value = value * 16 + nibble;
        }
        this.ascii(4); return value;
      }
      default: return this.error(location, "invalid string escape");
    }
  }
  private string(location: Location | undefined): string {
    this.ascii();
    const start = this.cursor;
    const startByte = this.byteCursor;
    let payloadBytes = 0;
    // The common unescaped path avoids per-character escape dispatch and rescanning.
    while (this.cursor < this.text.length) {
      const point = this.text.charCodeAt(this.cursor);
      if (point === 34) {
        this.reserve(payloadBytes, location);
        this.byteCursor += payloadBytes;
        const result = this.text.slice(start, this.cursor);
        this.context.stats.applicationAllocations++;
        this.ascii(); return result;
      }
      if (point === 92) break;
      if (point < 32) this.error(location, "unescaped control character");
      if (point >= 0xdc00 && point <= 0xdfff) this.error(location, "unpaired Unicode surrogate");
      if (point >= 0xd800 && point <= 0xdbff) {
        const low = this.text.charCodeAt(this.cursor + 1);
        if (!(low >= 0xdc00 && low <= 0xdfff)) this.error(location, "unpaired Unicode surrogate");
        payloadBytes += 4; this.cursor += 2;
      } else { payloadBytes += point < 128 ? 1 : point < 2048 ? 2 : 3; this.cursor++; }
    }
    this.cursor = start;
    payloadBytes = 0;
    let escaped = false;
    while (this.text[this.cursor] !== '"') {
      if (this.text[this.cursor] === "\\") escaped = true;
      const first = this.unit(location);
      if (first >= 0xdc00 && first <= 0xdfff) this.error(location, "unpaired Unicode surrogate");
      if (first >= 0xd800 && first <= 0xdbff) {
        const second = this.unit(location);
        if (second < 0xdc00 || second > 0xdfff) this.error(location, "unpaired Unicode surrogate");
        payloadBytes += 4;
      } else payloadBytes += first < 128 ? 1 : first < 2048 ? 2 : 3;
    }
    this.reserve(payloadBytes, location);
    const end = this.cursor;
    if (!escaped) {
      this.ascii(); this.context.stats.applicationAllocations++;
      return this.text.slice(start, end);
    }
    // One charged buffer and one decoded string even for an escape-only token.
    // The first pass establishes size/Unicode validity before either allocation.
    const buffer = new Uint8Array(payloadBytes);
    this.context.stats.applicationAllocations += 2;
    this.cursor = start; this.byteCursor = startByte;
    let offset = 0;
    while (this.cursor < end) {
      let point = this.unit(location);
      if (point >= 0xd800 && point <= 0xdbff) point = 0x10000 + (point - 0xd800) * 0x400 + this.unit(location) - 0xdc00;
      if (point < 0x80) buffer[offset++] = point;
      else if (point < 0x800) { buffer[offset++] = 0xc0 | point >> 6; buffer[offset++] = 0x80 | point & 0x3f; }
      else if (point < 0x10000) { buffer[offset++] = 0xe0 | point >> 12; buffer[offset++] = 0x80 | point >> 6 & 0x3f; buffer[offset++] = 0x80 | point & 0x3f; }
      else { buffer[offset++] = 0xf0 | point >> 18; buffer[offset++] = 0x80 | point >> 12 & 0x3f; buffer[offset++] = 0x80 | point >> 6 & 0x3f; buffer[offset++] = 0x80 | point & 0x3f; }
    }
    this.ascii(); return UTF8_DECODER.decode(buffer);
  }
  private value(parent: Location | undefined, depth: number, key?: string | number): JsonValue {
    let location = parent;
    this.whitespace();
    this.reserve(32, location); this.context.stats.tokens++;
    const start = this.byteCursor;
    const token = this.text[this.cursor];
    if (token === '"') { this.height = 0; return this.string(location); }
    if (token === "{" || token === "[") {
      if (depth > this.limits.maxDepth) this.error(location, `JSON depth limit exceeded (${this.limits.maxDepth})`);
      // Immutable path links are retained only for authenticated containers.
      // Scalar failures use the bounded shared stack, avoiding one object per scalar.
      if (key !== undefined) { location = { parent, key }; this.context.stats.applicationAllocations++; }
      const isArray = token === "[";
      const container: JsonValue[] | Record<string, JsonValue> = isArray ? [] : Object.create(null) as Record<string, JsonValue>;
      this.context.stats.applicationAllocations += 2;
      let height = 1;
      this.ascii(); this.whitespace();
      const close = isArray ? "]" : "}";
      if (this.text[this.cursor] !== close) {
        let index = 0;
        while (true) {
          let key: string | number = index;
          if (!isArray) {
            if (this.text[this.cursor] !== '"') this.error(location, "expected object key");
            key = this.string(location);
          }
          this.path.push(key);
          if (!isArray) {
            if (Object.hasOwn(container, key)) this.error(location, "duplicate object key");
            this.reserve(16, location);
            this.whitespace();
            if (this.text[this.cursor] !== ":") this.error(location, "expected colon");
            this.ascii();
          }
          const child = this.value(location, depth + 1, key);
          height = Math.max(height, this.height + 1);
          if (isArray) (container as JsonValue[]).push(child);
          else (container as Record<string, JsonValue>)[key] = child;
          this.path.pop();
          index++; this.whitespace();
          if (this.text[this.cursor] === close) break;
          if (this.text[this.cursor] !== ",") this.error(location, `expected comma or ${close}`);
          this.ascii(); this.whitespace();
        }
      }
      this.ascii();
      Object.freeze(container);
      origins.set(container, { context: this.context, start, end: this.byteCursor, depth: height, location });
      this.height = height;
      return container;
    }
    this.height = 0;
    if (token === "t" && this.text.startsWith("true", this.cursor)) { this.ascii(4); return true; }
    if (token === "f" && this.text.startsWith("false", this.cursor)) { this.ascii(5); return false; }
    if (token === "n" && this.text.startsWith("null", this.cursor)) { this.ascii(4); return null; }
    this.number.lastIndex = this.cursor;
    const match = this.number.exec(this.text);
    if (!match) return this.error(location, "expected JSON value");
    const value = Number(match[0]);
    if (!Number.isFinite(value)) this.error(location, "expected finite JSON number");
    // Charge bounded canonicalization scratch before allocating its temporary decimal strings.
    if (this.numberPolicy === "exact_decimal") this.reserve(Math.min(match[0].length, 384) * 8 + 256, location);
    if (this.numberPolicy === "exact_decimal" && (match[0].length > 384 || Math.abs(value) > Number.MAX_SAFE_INTEGER || decimalIdentity(match[0]) !== decimalIdentity(String(value)))) {
      this.error(location, "number cannot round-trip through the record decimal contract");
    }

    this.context.stats.applicationAllocations++;
    this.ascii(match[0].length);
    return value;
  }
  parse(): JsonValue {
    const result = this.value(undefined, 1);
    this.whitespace();
    if (this.cursor !== this.text.length) this.error(undefined, "trailing JSON input");
    return result;
  }
}

/** Parse the entire bounded response once. All containers are frozen and authenticated privately. */
export function parseBoundedJson(input: string | Uint8Array, requested: ReaderLimits = NO_LIMITS, policy: JsonNumberPolicy = "finite"): JsonValue {
  numberPolicy(policy);
  const limits = lowerLimits(RESPONSE_LIMITS, requested);
  let text: string;
  let bytes: number;
  if (typeof input === "string") {
    // UTF-8 needs at least one byte per UTF-16 code unit. Reject this lower
    // bound before scanning an arbitrarily large caller-owned string.
    if (input.length > limits.maxBytes) fail("$", `response byte limit exceeded (${limits.maxBytes})`);
    bytes = utf8Length(input); text = input;
  }
  else {
    // Internal-slot checks do not execute proxy/getter hooks or custom prototypes.
    if (!ArrayBuffer.isView(input) || typedArrayTag.call(input) !== "Uint8Array") fail("$", "expected JSON text or UTF-8 bytes");
    bytes = typedArrayByteLength.call(input) as number;
    if (bytes > limits.maxBytes) fail("$", `response byte limit exceeded (${limits.maxBytes})`);
    try { text = UTF8_DECODER.decode(input); }
    catch { fail("$", "invalid UTF-8 response"); }
  }
  if (bytes > limits.maxBytes) fail("$", `response byte limit exceeded (${limits.maxBytes})`);
  return new Reader(text, limits, bytes, typeof input === "string" ? 0 : 1, policy).parse();
}

/** Accumulate decompressed fetch bytes under the cap; never call unbounded response.text(). */
export async function readResponseJson(response: Response, requested: ReaderLimits = NO_LIMITS, policy: JsonNumberPolicy = "finite"): Promise<JsonValue> {
  numberPolicy(policy);
  const limits = lowerLimits(RESPONSE_LIMITS, requested);
  if (response.body === null) fail("$", "response body is missing");
  const reader = response.body.getReader();
  let buffer = new Uint8Array(Math.min(65536, limits.maxBytes));
  let used = 0;
  try {
    while (true) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value.byteLength > limits.maxBytes - used) fail("$", `response byte limit exceeded (${limits.maxBytes})`);
      const needed = used + chunk.value.byteLength;
      if (needed > buffer.length) {
        const replacement = new Uint8Array(Math.min(limits.maxBytes, Math.max(needed, buffer.length * 2)));
        replacement.set(buffer.subarray(0, used)); buffer = replacement;
      }
      buffer.set(chunk.value, used); used = needed;
    }
    return parseBoundedJson(buffer.subarray(0, used), limits, policy);
  } catch (error) {
    try { await reader.cancel(error); }
    catch (cancelError) { throw new AggregateError([error, cancelError], "JSON response failed and stream cancellation failed"); }
    throw error;
  } finally { reader.releaseLock(); }
}

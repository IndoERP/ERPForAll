/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { WireError } from "./errors";
import type { WireLimits } from "./limits";

/** An integer literal that fits `i64` or `u64`; larger literals and any literal with a fraction or exponent are floats. */
export interface JsonInt {
  readonly t: "int";
  readonly v: bigint;
}
/** A finite float. */
export interface JsonFloat {
  readonly t: "float";
  readonly v: number;
}
/** An object kept in document order; duplicate keys never reach this form. */
export interface JsonObject {
  readonly t: "object";
  readonly m: readonly (readonly [string, Json])[];
}
/** A parsed JSON value. */
export type Json = null | boolean | string | readonly Json[] | JsonInt | JsonFloat | JsonObject;

const NUMBER = /-?(?:0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?/y;
const I64_MIN = -(1n << 63n);
const U64_MAX = (1n << 64n) - 1n;

/**
 * The member of an object.
 *
 * @param value - any parsed value
 * @param key - the member name
 * @returns the member, or `undefined` when `value` is not an object or has no such member
 */
export function getMember(value: Json, key: string): Json | undefined {
  if (value === null || typeof value !== "object" || Array.isArray(value) || (value as JsonObject).t !== "object") {
    return undefined;
  }
  for (const [k, v] of (value as JsonObject).m) if (k === key) return v;
  return undefined;
}

/**
 * Whether a parsed value is an object.
 *
 * @param value - any parsed value
 */
export function isObject(value: Json | undefined): value is JsonObject {
  return value !== null && value !== undefined && typeof value === "object" && !Array.isArray(value) && (value as JsonObject).t === "object";
}

/**
 * Whether a parsed value is an array.
 *
 * @param value - any parsed value
 */
export function isArray(value: Json | undefined): value is readonly Json[] {
  return Array.isArray(value);
}

/**
 * Whether a parsed value is a number (integer or float literal).
 *
 * @param value - any parsed value
 */
export function isNumber(value: Json | undefined): value is JsonInt | JsonFloat {
  return value !== null && value !== undefined && typeof value === "object" && !Array.isArray(value) && ((value as JsonInt).t === "int" || (value as JsonFloat).t === "float");
}

function malformed(reason: string): WireError {
  return new WireError("malformed", "", reason);
}

/** True when the document nests deeper than `max`, counted outside strings; stops as soon as `max` is exceeded. */
function depthExceeds(bytes: Uint8Array, max: number): boolean {
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (const b of bytes) {
    if (inString) {
      if (escaped) escaped = false;
      else if (b === 0x5c) escaped = true;
      else if (b === 0x22) inString = false;
      continue;
    }
    if (b === 0x22) inString = true;
    else if (b === 0x7b || b === 0x5b) {
      depth += 1;
      if (depth > max) return true;
    } else if ((b === 0x7d || b === 0x5d) && depth > 0) depth -= 1;
  }
  return false;
}

class Reader {
  #pos = 0;
  constructor(private readonly text: string) {}

  value(): Json {
    this.#ws();
    const out = this.#any();
    this.#ws();
    if (this.#pos !== this.text.length) throw malformed("trailing characters");
    return out;
  }

  #ws(): void {
    for (;;) {
      const c = this.text[this.#pos];
      if (c === " " || c === "\t" || c === "\n" || c === "\r") this.#pos += 1;
      else return;
    }
  }

  #any(): Json {
    const c = this.text[this.#pos];
    if (c === "{") return this.#object();
    if (c === "[") return this.#array();
    if (c === '"') return this.#string();
    if (c === "-" || (c !== undefined && c >= "0" && c <= "9")) return this.#number();
    for (const [word, value] of [["true", true], ["false", false], ["null", null]] as const) {
      if (this.text.startsWith(word, this.#pos)) {
        this.#pos += word.length;
        return value;
      }
    }
    throw malformed(c === undefined ? "unexpected end of input" : `unexpected character at offset ${this.#pos}`);
  }

  #object(): JsonObject {
    this.#pos += 1;
    const members: [string, Json][] = [];
    const seen = new Set<string>();
    this.#ws();
    if (this.text[this.#pos] === "}") {
      this.#pos += 1;
      return { t: "object", m: members };
    }
    for (;;) {
      this.#ws();
      if (this.text[this.#pos] !== '"') throw malformed("expected a string key");
      const key = this.#string();
      if (seen.has(key)) throw malformed(`duplicate key ${JSON.stringify(key)}`);
      seen.add(key);
      this.#ws();
      if (this.text[this.#pos] !== ":") throw malformed("expected ':'");
      this.#pos += 1;
      this.#ws();
      members.push([key, this.#any()]);
      this.#ws();
      const c = this.text[this.#pos];
      this.#pos += 1;
      if (c === "}") return { t: "object", m: members };
      if (c !== ",") throw malformed("expected ',' or '}'");
    }
  }

  #array(): Json[] {
    this.#pos += 1;
    const items: Json[] = [];
    this.#ws();
    if (this.text[this.#pos] === "]") {
      this.#pos += 1;
      return items;
    }
    for (;;) {
      this.#ws();
      items.push(this.#any());
      this.#ws();
      const c = this.text[this.#pos];
      this.#pos += 1;
      if (c === "]") return items;
      if (c !== ",") throw malformed("expected ',' or ']'");
    }
  }

  #string(): string {
    this.#pos += 1;
    let out = "";
    for (;;) {
      const c = this.text[this.#pos];
      if (c === undefined) throw malformed("unterminated string");
      this.#pos += 1;
      if (c === '"') return out;
      if (c < " ") throw malformed("control character in string");
      if (c !== "\\") {
        out += c;
        continue;
      }
      const e = this.text[this.#pos];
      this.#pos += 1;
      switch (e) {
        case '"': out += '"'; break;
        case "\\": out += "\\"; break;
        case "/": out += "/"; break;
        case "b": out += "\b"; break;
        case "f": out += "\f"; break;
        case "n": out += "\n"; break;
        case "r": out += "\r"; break;
        case "t": out += "\t"; break;
        case "u": out += this.#unicode(); break;
        default: throw malformed("invalid escape");
      }
    }
  }

  #hex4(): number {
    const digits = this.text.slice(this.#pos, this.#pos + 4);
    if (!/^[0-9a-fA-F]{4}$/.test(digits)) throw malformed("invalid unicode escape");
    this.#pos += 4;
    return Number.parseInt(digits, 16);
  }

  #unicode(): string {
    const first = this.#hex4();
    if (first >= 0xdc00 && first <= 0xdfff) throw malformed("lone trailing surrogate");
    if (first < 0xd800 || first > 0xdbff) return String.fromCharCode(first);
    if (this.text[this.#pos] !== "\\" || this.text[this.#pos + 1] !== "u") throw malformed("lone leading surrogate");
    this.#pos += 2;
    const second = this.#hex4();
    if (second < 0xdc00 || second > 0xdfff) throw malformed("lone leading surrogate");
    return String.fromCharCode(first, second);
  }

  #number(): JsonInt | JsonFloat {
    NUMBER.lastIndex = this.#pos;
    const m = NUMBER.exec(this.text);
    if (m === null) throw malformed("invalid number");
    const literal = m[0];
    this.#pos += literal.length;
    if (m[1] === undefined && m[2] === undefined && literal !== "-0") {
      const v = BigInt(literal);
      if (v >= I64_MIN && v <= U64_MAX) return { t: "int", v };
    }
    const v = Number(literal);
    if (!Number.isFinite(v)) throw malformed("number out of range");
    return { t: "float", v };
  }
}

/**
 * Parse one document under the size and depth limits. Strict: duplicate object keys are refused (never
 * last-one-wins), integers keep full precision, objects keep document order, invalid UTF-8 and a byte-order mark
 * are refused.
 *
 * @param input - the document as text or UTF-8 bytes
 * @param limits - the host's limits
 * @throws {@link WireError} `limit_exceeded` (bytes, depth) or `malformed` with an empty path
 */
export function parseJson(input: string | Uint8Array, limits: WireLimits): Json {
  const bytes = typeof input === "string" ? new TextEncoder().encode(input) : input;
  limits.check("bytes", bytes.length, "");
  if (depthExceeds(bytes, limits.get("depth"))) {
    throw new WireError("limit_exceeded", "", `limit depth exceeded (maximum ${limits.get("depth")})`, "depth");
  }
  let text: string;
  try {
    text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
  } catch {
    throw malformed("invalid UTF-8");
  }
  return new Reader(text).value();
}

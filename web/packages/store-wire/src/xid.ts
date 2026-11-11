/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { WireError } from "./errors";

const U64_MAX = (1n << 64n) - 1n;

/**
 * Parse a canonical decimal: ASCII digits only, no sign, no leading zero. Zero itself is accepted here; callers
 * that forbid it check the value.
 *
 * @param text - the text
 * @returns the number, or the reason it is not canonical
 */
export function parseDecimal(text: string): bigint | string {
  if (text === "") return "empty";
  if (!/^[0-9]+$/.test(text)) return "not a decimal digit";
  if (text.length > 1 && text.startsWith("0")) return "leading zero";
  const value = BigInt(text);
  return value > U64_MAX ? "above 2^64-1" : value;
}

/**
 * Parse a PostgreSQL `xid8` in its canonical wire form (1..2^64-1).
 *
 * @param text - the wire string
 * @param path - the path reported by a refusal (default `xid`)
 * @throws {@link WireError} `bad_descriptor` for an empty string, a sign or other non-digit, a leading zero, zero,
 * or a value above 2^64-1
 */
export function parseXid(text: string, path = "xid"): bigint {
  const value = parseDecimal(text);
  if (typeof value === "string") throw new WireError("bad_descriptor", path, value);
  if (value === 0n) throw new WireError("bad_descriptor", path, "zero is not an xid8");
  return value;
}

/**
 * The canonical wire form of an `xid8`.
 *
 * @param xid - the value
 */
export function formatXid(xid: bigint): string {
  return xid.toString(10);
}

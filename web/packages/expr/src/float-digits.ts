/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Correctly rounded decimal digits of a finite, non-negative double, for the `f`, `e`, `g` and `%`
 * presentation types.
 *
 * Python formats the exact binary value of a float and rounds half to even only on an exact tie, so
 * `'%.0f' % 0.5` is `0` and `'%.2f' % 2.675` is `2.67` (the binary value is just below the tie). JS
 * `toFixed`/`toExponential` round ties away from zero, switch to exponent notation at 1e21 and throw
 * a native `RangeError` above precision 100, so they are not used here. The value is decomposed into
 * an integer ratio `num / den` (den a power of two) and rounded with BigInt arithmetic.
 *
 * Work is bounded by the caller: precision is at most `EvalLimits.maxFormatWidth` (checked by
 * `Budget.checkFormatWidth` while the spec is parsed) and the decimal exponent of a double lies in
 * [-324, 308], so no BigInt here has more than roughly 1,400 decimal digits. The caller charges the
 * predicted digit count to the budget before calling (see `predictedDigits`).
 */

interface Ratio {
  readonly num: bigint;
  readonly den: bigint;
}

/** `x` (finite, >= 0) as an exact ratio. */
function exactRatio(x: number): Ratio {
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, x);
  const bits = view.getBigUint64(0);
  const biased = Number((bits >> 52n) & 0x7ffn);
  const frac = bits & 0xfffffffffffffn;
  // Subnormals (biased exponent 0) have no implicit leading bit and exponent 1 - 1023.
  const mantissa = biased === 0 ? frac : frac | (1n << 52n);
  const exp2 = (biased === 0 ? 1 : biased) - 1075;
  return exp2 >= 0 ? { num: mantissa << BigInt(exp2), den: 1n } : { num: mantissa, den: 1n << BigInt(-exp2) };
}

/** `num / den` rounded to an integer, half to even. */
function roundHalfEven(num: bigint, den: bigint): bigint {
  const q = num / den;
  const twice = (num % den) * 2n;
  if (twice > den || (twice === den && (q & 1n) === 1n)) return q + 1n;
  return q;
}

function pow10(n: number): bigint {
  return 10n ** BigInt(n);
}

/** `r * 10^k` rounded half to even (k may be negative). */
function scaledRound(r: Ratio, k: number): bigint {
  return k >= 0 ? roundHalfEven(r.num * pow10(k), r.den) : roundHalfEven(r.num, r.den * pow10(-k));
}

/** Whether `r >= 10^e`. */
function atLeastPow10(r: Ratio, e: number): boolean {
  return e >= 0 ? r.num >= r.den * pow10(e) : r.num * pow10(-e) >= r.den;
}

function bitLength(n: bigint): number {
  return n === 0n ? 0 : n.toString(2).length;
}

/** floor(log10(r)) for r > 0, exact. */
function decimalExponent(r: Ratio): number {
  let e = Math.floor((bitLength(r.num) - bitLength(r.den)) * Math.LOG10E * Math.LN2);
  while (!atLeastPow10(r, e)) e -= 1;
  while (atLeastPow10(r, e + 1)) e += 1;
  return e;
}

/** An upper bound of the number of digits `fixedDigits`/`expDigits` produce, for budget charging. */
export function predictedDigits(x: number, precision: number): number {
  // A double is below 1.8e308, so at most 309 integer digits.
  const intDigits = x >= 1 ? Math.min(309, Math.floor(Math.log10(x)) + 2) : 1;
  return intDigits + precision + 8;
}

/**
 * Fixed notation with exactly `precision` digits after the point: `"I"` or `"I.FFF"`. With `alt`
 * and precision 0 the point is kept (`"1."`).
 */
export function fixedDigits(x: number, precision: number, alt: boolean): string {
  const n = scaledRound(exactRatio(x), precision);
  const digits = n.toString().padStart(precision + 1, "0");
  const intPart = digits.slice(0, digits.length - precision);
  const fracPart = digits.slice(digits.length - precision);
  if (precision === 0) return alt ? `${intPart}.` : intPart;
  return `${intPart}.${fracPart}`;
}

/** `precision + 1` significant digits and the decimal exponent of the first one, after rounding. */
export function expDigits(x: number, precision: number): { digits: string; exponent: number } {
  if (x === 0) return { digits: "0".repeat(precision + 1), exponent: 0 };
  const r = exactRatio(x);
  let exponent = decimalExponent(r);
  let n = scaledRound(r, precision - exponent);
  // Rounding 9.99… up gives 10^(precision+1): one more decimal exponent, and the digits are 10…0.
  if (n === pow10(precision + 1)) {
    n /= 10n;
    exponent += 1;
  }
  return { digits: n.toString(), exponent };
}

/** Python's exponent suffix: `e+05`, `E-10`, at least two exponent digits. */
export function exponentSuffix(exponent: number, upper: boolean): string {
  const sign = exponent < 0 ? "-" : "+";
  return `${upper ? "E" : "e"}${sign}${Math.abs(exponent).toString().padStart(2, "0")}`;
}

/** Exponent notation with `precision` digits after the point. */
export function exponentForm(x: number, precision: number, upper: boolean, alt: boolean): string {
  const { digits, exponent } = expDigits(x, precision);
  const point = precision > 0 || alt ? "." : "";
  return `${digits[0]!}${point}${digits.slice(1)}${exponentSuffix(exponent, upper)}`;
}

function stripTrailingZeros(mantissa: string): string {
  if (!mantissa.includes(".")) return mantissa;
  return mantissa.replace(/0+$/, "").replace(/\.$/, "");
}

/**
 * The `g` type (and `n`, and the no-type form with a precision when `reprStyle`): `precision`
 * significant digits (0 means 1). Fixed notation when the rounded exponent X satisfies
 * -4 <= X < P (repr style: X < P - 1), exponent notation otherwise. Without `alt`, trailing zeros and
 * a trailing point are removed; with `alt` they stay and a point is always present. Repr style adds
 * `.0` to an integral fixed result.
 */
export function generalForm(x: number, precision: number, upper: boolean, alt: boolean, reprStyle: boolean): string {
  const p = precision === 0 ? 1 : precision;
  const { exponent } = expDigits(x, p - 1);
  const limit = reprStyle ? p - 1 : p;
  let out: string;
  if (exponent >= -4 && exponent < limit) {
    out = fixedDigits(x, p - 1 - exponent, alt);
    if (!alt) out = stripTrailingZeros(out);
    if (reprStyle && !out.includes(".")) out += ".0";
  } else {
    const form = exponentForm(x, p - 1, upper, alt);
    const idx = form.search(/[eE]/);
    const mantissa = form.slice(0, idx);
    out = `${alt ? mantissa : stripTrailingZeros(mantissa)}${form.slice(idx)}`;
  }
  return out;
}

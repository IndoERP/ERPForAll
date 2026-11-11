/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Python `str` character rules the JS built-ins do not match.
 *
 * - Whitespace: Python's `str.isspace()` set (used by argument-less `split()` and `strip()`), which
 *   includes U+001C..U+001F and U+0085 and excludes U+FEFF, unlike JS `\s`.
 * - Titlecase: `capitalize()` titlecases the first character. For most characters the titlecase is
 *   the first character of the uppercase mapping followed by the lowercase of the rest of it
 *   (`ß` -> `Ss`, `ﬁ` -> `Fi`). `TITLE_EXCEPTIONS` lists every code point where that rule and
 *   CPython 3.12.15 disagree (digraphs, Georgian, Greek with ypogegrammeni, `ŉ`), recorded the same
 *   way. Upper/lowercase mappings come from the JS engine; a JS engine on a different Unicode
 *   version than CPython's 15.0 can differ for newly encoded characters (recorded divergence,
 *   ).
 */

const PY_WHITESPACE: ReadonlySet<number> = new Set([
  0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x85, 0xa0, 0x1680, 0x2000, 0x2001,
  0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200a, 0x2028, 0x2029, 0x202f,
  0x205f, 0x3000,
]);

/** Whether the single code point `ch` is whitespace for Python. */
export function isPySpace(ch: string): boolean {
  const cp = ch.codePointAt(0);
  return cp !== undefined && PY_WHITESPACE.has(cp);
}

function range(from: number, to: number, map: (cp: number) => number[]): [number, number[]][] {
  const out: [number, number[]][] = [];
  for (let cp = from; cp <= to; cp++) out.push([cp, map(cp)]);
  return out;
}

const TITLE_EXCEPTIONS: ReadonlyMap<number, string> = new Map(
  (
    [
      [0x0149, [0x02bc, 0x004e]],
      [0x01c4, [0x01c5]],
      [0x01c5, [0x01c5]],
      [0x01c6, [0x01c5]],
      [0x01c7, [0x01c8]],
      [0x01c8, [0x01c8]],
      [0x01c9, [0x01c8]],
      [0x01ca, [0x01cb]],
      [0x01cb, [0x01cb]],
      [0x01cc, [0x01cb]],
      [0x01f1, [0x01f2]],
      [0x01f2, [0x01f2]],
      [0x01f3, [0x01f2]],
      // Georgian Mkhedruli: titlecase is the letter itself (uppercase is Mtavruli).
      ...range(0x10d0, 0x10fa, (cp) => [cp]),
      ...range(0x10fd, 0x10ff, (cp) => [cp]),
      // Greek with ypogegrammeni: the titlecase is the prosgegrammeni form.
      ...range(0x1f80, 0x1f87, (cp) => [cp + 8]),
      ...range(0x1f88, 0x1f8f, (cp) => [cp]),
      ...range(0x1f90, 0x1f97, (cp) => [cp + 8]),
      ...range(0x1f98, 0x1f9f, (cp) => [cp]),
      ...range(0x1fa0, 0x1fa7, (cp) => [cp + 8]),
      ...range(0x1fa8, 0x1faf, (cp) => [cp]),
      [0x1fb2, [0x1fba, 0x0345]],
      [0x1fb3, [0x1fbc]],
      [0x1fb4, [0x0386, 0x0345]],
      [0x1fb7, [0x0391, 0x0342, 0x0345]],
      [0x1fbc, [0x1fbc]],
      [0x1fc2, [0x1fca, 0x0345]],
      [0x1fc3, [0x1fcc]],
      [0x1fc4, [0x0389, 0x0345]],
      [0x1fc7, [0x0397, 0x0342, 0x0345]],
      [0x1fcc, [0x1fcc]],
      [0x1ff2, [0x1ffa, 0x0345]],
      [0x1ff3, [0x1ffc]],
      [0x1ff4, [0x038f, 0x0345]],
      [0x1ff7, [0x03a9, 0x0342, 0x0345]],
      [0x1ffc, [0x1ffc]],
    ] as [number, number[]][]
  ).map(([cp, title]) => [cp, String.fromCodePoint(...title)]),
);

/** Python's titlecase mapping of the single code point `ch` (may be several code points). */
export function pyTitle(ch: string): string {
  const cp = ch.codePointAt(0);
  if (cp === undefined) return "";
  const exception = TITLE_EXCEPTIONS.get(cp);
  if (exception !== undefined) return exception;
  const upper = Array.from(ch.toUpperCase());
  if (upper.length === 0) return "";
  return upper[0]! + upper.slice(1).join("").toLowerCase();
}

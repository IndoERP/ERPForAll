/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import {
  createContext,
  decodeAst,
  evalAst,
  intVal,
  floatVal,
  strVal,
  bytesVal,
  listVal,
  tupleVal,
  dictVal,
  boolVal,
  NONE,
  parseBoundedJson,
  repr,
  relativeDeltaVal,
  formatValue,
  printf,
  Budget,
  PythonError,
  callableVal,
  objectVal,
  type ExprValue,
  type ValidatedAst,
} from "../src/index";

import { intToFloat } from "../src/format";

function floatBits(v: number): string {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  view.setFloat64(0, v);
  return view.getBigUint64(0).toString(16).padStart(16, "0");
}

function decode(obj: unknown): ValidatedAst {
  return decodeAst(parseBoundedJson(JSON.stringify(obj)));
}

/// (partial: repr builtin quote selection and escapes)
test("evaluates Python repr builtin and escape rules", () => {
  const ctx = createContext();

  // repr('it\'s') -> '"it\'s"'
  const ast1 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "str", value: "it's" } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast1, ctx)).toEqual(strVal('"it\'s"'));

  // repr('say "hi"') -> '\'say "hi"\''
  const ast2 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "str", value: 'say "hi"' } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast2, ctx)).toEqual(strVal("'say \"hi\"'"));

  // repr('both \' and "') -> '\'both \\\' and "\''
  const ast3 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "str", value: 'both \' and "' } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast3, ctx)).toEqual(strVal("'both \\' and \"'"));

  // repr('tab\tnew\nline\\') -> '\'tab\\tnew\\nline\\\\\''
  const ast4 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "str", value: "tab\tnew\nline\\" } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast4, ctx)).toEqual(strVal("'tab\\tnew\\nline\\\\'"));

  // repr('é') -> "'é'"
  const ast5 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "str", value: "é" } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast5, ctx)).toEqual(strVal("'é'"));

  // repr(b'a\x00\xff') -> "b'a\\x00\\xff'"
  const ast6 = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "literal", value: { kind: "bytes", value: [0x61, 0x00, 0xff] } },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
  expect(evalAst(ast6, ctx)).toEqual(strVal("b'a\\x00\\xff'"));

  // repr takes exactly 1 positional argument
  const astErr = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "repr" },
      { kind: "call", func: 0, args: [], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(astErr, ctx))).toEqual({ cls: "TypeError", msg: "repr() takes exactly one argument (0 given)" });
});

/// (partial: str split/join/strip/capitalize/replace/startswith basic values)
test("evaluates basic string method values", () => {
  const ctx = createContext();

  // 'a,b,,c'.split(',')
  const astSplit1 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "a,b,,c" } },
      { kind: "attribute", value: 0, attr: "split" },
      { kind: "literal", value: { kind: "str", value: "," } },
      { kind: "call", func: 1, args: [2], keywords: [] },
    ],
  });
  expect(evalAst(astSplit1, ctx)).toEqual(listVal([strVal("a"), strVal("b"), strVal(""), strVal("c")]));

  // 'a b  c '.split()
  const astSplit2 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "a b  c " } },
      { kind: "attribute", value: 0, attr: "split" },
      { kind: "call", func: 1, args: [], keywords: [] },
    ],
  });
  expect(evalAst(astSplit2, ctx)).toEqual(listVal([strVal("a"), strVal("b"), strVal("c")]));

  // '  a b  c '.split(None, 1)
  const astSplit3 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "  a b  c " } },
      { kind: "attribute", value: 0, attr: "split" },
      { kind: "literal", value: { kind: "none" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "call", func: 1, args: [2, 3], keywords: [] },
    ],
  });
  expect(evalAst(astSplit3, ctx)).toEqual(listVal([strVal("a"), strVal("b  c ")]));

  // 'abc'.split('') -> ValueError: empty separator
  const astSplitErr = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "abc" } },
      { kind: "attribute", value: 0, attr: "split" },
      { kind: "literal", value: { kind: "str", value: "" } },
      { kind: "call", func: 1, args: [2], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(astSplitErr, ctx))).toEqual({ cls: "ValueError", msg: "empty separator" });

  // '-'.join(['a', 'b', 'c'])
  const astJoin = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "-" } },
      { kind: "attribute", value: 0, attr: "join" },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "literal", value: { kind: "str", value: "b" } },
      { kind: "literal", value: { kind: "str", value: "c" } },
      { kind: "list", items: [2, 3, 4] },
      { kind: "call", func: 1, args: [5], keywords: [] },
    ],
  });
  expect(evalAst(astJoin, ctx)).toEqual(strVal("a-b-c"));

  // '-'.join([1, 2]) -> TypeError
  const astJoinErr = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "-" } },
      { kind: "attribute", value: 0, attr: "join" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "list", items: [2] },
      { kind: "call", func: 1, args: [3], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(astJoinErr, ctx))).toEqual({ cls: "TypeError", msg: "sequence item 0: expected str instance, int found" });

  // strip, lstrip, rstrip
  const astStrip = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "xxaxx" } },
      { kind: "attribute", value: 0, attr: "strip" },
      { kind: "literal", value: { kind: "str", value: "x" } },
      { kind: "call", func: 1, args: [2], keywords: [] },
    ],
  });
  expect(evalAst(astStrip, ctx)).toEqual(strVal("a"));

  // lower, upper, capitalize
  const astCap = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "hELLO wORLD" } },
      { kind: "attribute", value: 0, attr: "capitalize" },
      { kind: "call", func: 1, args: [], keywords: [] },
    ],
  });
  expect(evalAst(astCap, ctx)).toEqual(strVal("Hello world"));

  // replace
  const astRep = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "abc" } },
      { kind: "attribute", value: 0, attr: "replace" },
      { kind: "literal", value: { kind: "str", value: "" } },
      { kind: "literal", value: { kind: "str", value: "-" } },
      { kind: "call", func: 1, args: [2, 3], keywords: [] },
    ],
  });
  expect(evalAst(astRep, ctx)).toEqual(strVal("-a-b-c-"));

  // startswith and endswith
  const astStart = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "abc" } },
      { kind: "attribute", value: 0, attr: "startswith" },
      { kind: "literal", value: { kind: "str", value: "x" } },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "tuple", items: [2, 3] },
      { kind: "call", func: 1, args: [4], keywords: [] },
    ],
  });
  expect(evalAst(astStart, ctx)).toEqual(boolVal(true));
});

/// (partial: printf width, flags, mapping keys and errors)
test("evaluates printf '%' formatting with full specifiers, signs and errors", () => {
  const ctx = createContext();

  // '%5d|%-5d|%05d' % (42, 42, 42) -> '   42|42   |00042'
  const ast1 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "%5d|%-5d|%05d" } },
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "tuple", items: [1, 2, 3] },
      { kind: "binary", op: "mod", left: 0, right: 4 },
    ],
  });
  expect(evalAst(ast1, ctx)).toEqual(strVal("   42|42   |00042"));

  // '%(a)s-%(b)03d' % {'a': 'x', 'b': 7} -> 'x-007'
  const ast2 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "%(a)s-%(b)03d" } },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "literal", value: { kind: "str", value: "x" } },
      { kind: "literal", value: { kind: "str", value: "b" } },
      { kind: "literal", value: { kind: "int", value: "7" } },
      { kind: "dict", items: [[1, 2], [3, 4]] },
      { kind: "binary", op: "mod", left: 0, right: 5 },
    ],
  });
  expect(evalAst(ast2, ctx)).toEqual(strVal("x-007"));

  // '%d' % 'x' -> TypeError: %d format: a real number is required, not str
  const astErr1 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "%d" } },
      { kind: "literal", value: { kind: "str", value: "x" } },
      { kind: "binary", op: "mod", left: 0, right: 1 },
    ],
  });
  expect(pyError(() => evalAst(astErr1, ctx))).toEqual({ cls: "TypeError", msg: "%d format: a real number is required, not str" });

  // '%q' % 1 -> ValueError: unsupported format character 'q' (0x71) at index 1
  const astErr2 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "%q" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "binary", op: "mod", left: 0, right: 1 },
    ],
  });
  expect(pyError(() => evalAst(astErr2, ctx))).toEqual({ cls: "ValueError", msg: "unsupported format character 'q' (0x71) at index 1" });

  // '%c' % 1114112 -> OverflowError: %c arg not in range(0x110000)
  const astErr3 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "%c" } },
      { kind: "literal", value: { kind: "int", value: "1114112" } },
      { kind: "binary", op: "mod", left: 0, right: 1 },
    ],
  });
  expect(pyError(() => evalAst(astErr3, ctx))).toEqual({ cls: "OverflowError", msg: "%c arg not in range(0x110000)" });
});

/// (partial: str.format positional, auto-numbering and switch errors)
test("evaluates str.format() with positional, keyword, auto-numbering and error handling", () => {
  const ctx = createContext();

  // '{} {}'.format(1, 'a') -> '1 a'
  const ast1 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "{} {}" } },
      { kind: "attribute", value: 0, attr: "format" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "call", func: 1, args: [2, 3], keywords: [] },
    ],
  });
  expect(evalAst(ast1, ctx)).toEqual(strVal("1 a"));

  // '{0!r:>6}'.format('a') -> "   'a'"
  const ast2 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "{0!r:>6}" } },
      { kind: "attribute", value: 0, attr: "format" },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "call", func: 1, args: [2], keywords: [] },
    ],
  });
  expect(evalAst(ast2, ctx)).toEqual(strVal("   'a'"));

  // '{0} {}'.format(1, 2) -> ValueError: cannot switch from manual field specification to automatic field numbering
  const astErr = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "{0} {}" } },
      { kind: "attribute", value: 0, attr: "format" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "int", value: "2" } },
      { kind: "call", func: 1, args: [2, 3], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(astErr, ctx))).toEqual({ cls: "ValueError", msg: "cannot switch from manual field specification to automatic field numbering" });
});

/// (partial: f-string conversions, alignment and string sign error)
test("evaluates f-strings: JoinedStr and FormattedValue with conversions and format specs", () => {
  const ctx = createContext();

  // f'{"é"!a}' -> "'\\xe9'"
  const ast1 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "é" } },
      { kind: "formatted_value", value: 0, conversion: "a", format_spec: null },
      { kind: "joined_str", items: [1] },
    ],
  });
  expect(evalAst(ast1, ctx)).toEqual(strVal("'\\xe9'"));

  // f'{3.14159:.2f}' -> '3.14'
  const ast2 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "float", value: floatBits(3.14159) } },
      { kind: "formatted_value", value: 0, conversion: null, format_spec: ".2f" },
      { kind: "joined_str", items: [1] },
    ],
  });
  expect(evalAst(ast2, ctx)).toEqual(strVal("3.14"));

  // f'{42:>6}|{42:<6}|{42:^6}' -> '    42|42    |  42  '
  const ast3 = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "formatted_value", value: 0, conversion: null, format_spec: ">6" },
      { kind: "literal", value: { kind: "str", value: "|" } },
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "formatted_value", value: 3, conversion: null, format_spec: "<6" },
      { kind: "literal", value: { kind: "str", value: "|" } },
      { kind: "literal", value: { kind: "int", value: "42" } },
      { kind: "formatted_value", value: 6, conversion: null, format_spec: "^6" },
      { kind: "joined_str", items: [1, 2, 4, 5, 7] },
    ],
  });
  expect(evalAst(ast3, ctx)).toEqual(strVal("    42|42    |  42  "));

  // f'{"x":+}' -> ValueError: Sign not allowed in string format specifier
  const astErr = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "str", value: "x" } },
      { kind: "formatted_value", value: 0, conversion: null, format_spec: "+" },
      { kind: "joined_str", items: [1] },
    ],
  });
  expect(pyError(() => evalAst(astErr, ctx))).toEqual({ cls: "ValueError", msg: "Sign not allowed in string format specifier" });
});

/// (partial: format width over maxFormatWidth is LimitExceeded)
test("rejects a format width above maxFormatWidth with LimitExceeded", () => {
  const limits = { maxFormatWidth: 1000, maxValueBytes: 2000 };
  const ctx = createContext({}, limits);

  // format width > 1000 throws LimitExceeded
  const astWidth = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "formatted_value", value: 0, conversion: null, format_spec: "1001d" },
      { kind: "joined_str", items: [1] },
    ],
  });
  expect(pyError(() => evalAst(astWidth, ctx))).toEqual({ cls: "LimitExceeded", msg: "format width or precision 1001 exceeds the limit of 1000" });
});

/// (partial: str attribute lookup never reaches JS prototype properties)
test("preserves prototype safety with no JS property fallback or eval", () => {
  const ctx = createContext();

  // Accessing unpermitted attributes on string throws AttributeError, never returns Object.prototype property
  for (const prop of ["constructor", "valueOf", "toString", "__proto__", "charCodeAt"]) {
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "str", value: "hello" } },
        { kind: "attribute", value: 0, attr: prop },
      ],
    });
    expect(pyError(() => evalAst(ast, ctx))).toEqual({ cls: "AttributeError", msg: `'str' object has no attribute '${prop}'` });
  }
});

// ---------------------------------------------------------------------------------------------
// Review fix list ("Pre-merge delta check").

type Lit = null | boolean | bigint | number | string | { t: Lit[] } | { l: Lit[] } | { d: [Lit, Lit][] };

class Ast {
  readonly nodes: unknown[] = [];

  add(node: unknown): number {
    this.nodes.push(node);
    return this.nodes.length - 1;
  }

  lit(v: Lit): number {
    if (v === null) return this.add({ kind: "literal", value: { kind: "none" } });
    if (typeof v === "boolean") return this.add({ kind: "literal", value: { kind: "bool", value: v } });
    if (typeof v === "bigint") return this.add({ kind: "literal", value: { kind: "int", value: v.toString() } });
    if (typeof v === "number") return this.add({ kind: "literal", value: { kind: "float", value: floatBits(v) } });
    if (typeof v === "string") return this.add({ kind: "literal", value: { kind: "str", value: v } });
    if ("t" in v) {
      const items = v.t.map((x) => this.lit(x));
      return this.add({ kind: "tuple", items });
    }
    if ("l" in v) {
      const items = v.l.map((x) => this.lit(x));
      return this.add({ kind: "list", items });
    }
    const items = v.d.map(([k, x]) => [this.lit(k), this.lit(x)]);
    return this.add({ kind: "dict", items });
  }

  name(n: string): number {
    return this.add({ kind: "name", name: n });
  }

  /** `recv.attr(*args, **kw)`; a keyword with a null name is `**value`. */
  method(recv: Lit, attr: string, args: Lit[], kw: [string | null, Lit][] = []): number {
    const r = this.lit(recv);
    const f = this.add({ kind: "attribute", value: r, attr });
    const a = args.map((x) => this.lit(x));
    const k = kw.map(([name, x]) => ({ name, value: this.lit(x) }));
    return this.add({ kind: "call", func: f, args: a, keywords: k });
  }

  call(fn: string, args: number[]): number {
    const f = this.name(fn);
    return this.add({ kind: "call", func: f, args, keywords: [] });
  }
}

function run(build: (a: Ast) => void, names: Record<string, ExprValue> = {}): ExprValue {
  const a = new Ast();
  build(a);
  return evalAst(decode({ schema: 1, nodes: a.nodes }), createContext(names));
}

function m(recv: Lit, attr: string, args: Lit[], kw: [string | null, Lit][] = []): ExprValue {
  return run((a) => a.method(recv, attr, args, kw));
}

/** `template % arg` */
function pct(template: string, arg: Lit): ExprValue {
  return run((a) => {
    const l = a.lit(template);
    const r = a.lit(arg);
    a.add({ kind: "binary", op: "mod", left: l, right: r });
  });
}

/** `f"{value:spec}"` */
function fs(value: Lit, spec: string): ExprValue {
  return run((a) => {
    const v = a.lit(value);
    const fv = a.add({ kind: "formatted_value", value: v, conversion: null, format_spec: spec });
    a.add({ kind: "joined_str", items: [fv] });
  });
}

/** `f"{name:spec}"` over a host-provided name. */
function fsName(names: Record<string, ExprValue>, spec: string | null, conversion: string | null = null): ExprValue {
  return run((a) => {
    const v = a.name("v");
    const fv = a.add({ kind: "formatted_value", value: v, conversion, format_spec: spec });
    a.add({ kind: "joined_str", items: [fv] });
  }, names);
}

function pyError(fn: () => unknown): { cls: string; msg: string } {
  try {
    fn();
  } catch (e) {
    if (e instanceof PythonError) return { cls: e.className, msg: e.messageDetail };
    throw new Error(`expected a PythonError, got a native error: ${String(e)}`);
  }
  throw new Error("expected a PythonError, nothing was thrown");
}

function str(v: ExprValue): string {
  if (v.kind !== "str") throw new Error(`expected a str result, got ${v.kind}`);
  return v.value;
}

/// (partial: str.replace treats $ patterns in the replacement literally)
test("str.replace inserts the replacement text literally, without JS $ patterns", () => {
  expect(str(m("a", "replace", ["a", "$&$&"]))).toBe("$&$&");
  expect(str(m("a$b", "replace", ["$", "$$"]))).toBe("a$$b");
  expect(str(m("x", "replace", ["x", "$'$`"]))).toBe("$'$`");
  expect(str(m("aaa", "replace", ["a", "$&", 2n]))).toBe("$&$&a");
  expect(str(m("aaa", "replace", ["aa", "x"]))).toBe("xa");
  expect(str(m("aaa", "replace", ["a", "bb", -5n]))).toBe("bbbbbb");
  expect(str(m("ab", "replace", ["", "-", 2n]))).toBe("-a-b");
  expect(str(m("a", "replace", ["", "x", 0n]))).toBe("a");
  expect(str(m("a", "replace", ["a", "b", true]))).toBe("b");
});

/// (partial: str.replace output size is checked before the string is built)
test("str.replace refuses an oversized result before building it, on both match paths", () => {
  const limits = { maxValueBytes: 1000 };
  // 'aaaa…'.replace('a', 'x' * 400): 10 * 400 bytes predicted, over the 1000-byte per-value limit.
  const big = (old: string) => () =>
    evalAst(
      (() => {
        const a = new Ast();
        a.method("a".repeat(10), "replace", [old, "x".repeat(400)]);
        return decode({ schema: 1, nodes: a.nodes });
      })(),
      createContext({}, limits),
    );
  expect(pyError(big("a"))).toEqual({
    cls: "LimitExceeded",
    msg: "value of 4032 bytes exceeds the per-value limit of 1000 bytes",
  });
  // Empty `old` inserts at each of the 11 positions: 10 + 11 * 400 bytes.
  expect(pyError(big(""))).toEqual({
    cls: "LimitExceeded",
    msg: "value of 4442 bytes exceeds the per-value limit of 1000 bytes",
  });
});

/// (partial: str.replace argument errors use CPython 3.12 messages)
test("str.replace argument errors", () => {
  expect(pyError(() => m("abc", "replace", ["b", "x", 1n, 2n]))).toEqual({
    cls: "TypeError",
    msg: "replace expected at most 3 arguments, got 4",
  });
  expect(pyError(() => m("abc", "replace", ["b", "x"], [["count", 1n]]))).toEqual({
    cls: "TypeError",
    msg: "str.replace() takes no keyword arguments",
  });
  expect(pyError(() => m("a", "replace", ["a"]))).toEqual({
    cls: "TypeError",
    msg: "replace expected at least 2 arguments, got 1",
  });
  expect(pyError(() => m("a", "replace", ["a", "b", "x"]))).toEqual({
    cls: "TypeError",
    msg: "'str' object cannot be interpreted as an integer",
  });
  expect(pyError(() => m("a", "replace", ["a", "b", null]))).toEqual({
    cls: "TypeError",
    msg: "'NoneType' object cannot be interpreted as an integer",
  });
  expect(pyError(() => m("a", "replace", [1n, "b"]))).toEqual({
    cls: "TypeError",
    msg: "replace() argument 1 must be str, not int",
  });
});

/// (partial: str.format keyword lookup is own-key only, no JS prototype names)
test("str.format keyword names never resolve to JS prototype properties", () => {
  for (const key of ["constructor", "toString", "__proto__", "hasOwnProperty", "valueOf"]) {
    expect(pyError(() => m(`{${key}}`, "format", []))).toEqual({ cls: "KeyError", msg: `'${key}'` });
  }
  // `"{__proto__}".format(**{"__proto__": 1})` is an ordinary keyword.
  expect(str(m("{__proto__}", "format", [], [[null, { d: [["__proto__", 1n]] }]]))).toBe("1");
  // A `__proto__` keyword must not become the container's prototype (its `kind` field would leak).
  expect(pyError(() => m("{kind}", "format", [], [[null, { d: [["__proto__", 1n]] }]]))).toEqual({
    cls: "KeyError",
    msg: "'kind'",
  });
  expect(str(m("{constructor}-{a}", "format", [], [[null, { d: [["constructor", "c"]] }], ["a", "x"]]))).toBe("c-x");
  expect(pyError(() => m("{a}", "format", [], [[null, { d: [["a", 1n]] }], ["a", 2n]]))).toEqual({
    cls: "TypeError",
    msg: "str.format() got multiple values for keyword argument 'a'",
  });
});

/// (partial: duplicate keyword from ** names the callable like CPython 3.12)
test("a keyword given twice through ** names the callable", () => {
  const dup = (build: (a: Ast) => number, name: string) => {
    expect(
      pyError(() =>
        run((a) => {
          const call = build(a);
          void call;
        }),
      ),
    ).toEqual({ cls: "TypeError", msg: `${name}() got multiple values for keyword argument 'a'` });
  };
  const kw = (a: Ast) => [
    { name: null, value: a.lit({ d: [["a", 1n]] }) },
    { name: "a", value: a.lit(2n) },
  ];
  const builtin = (fn: string) => (a: Ast) => {
    const f = a.name(fn);
    const k = kw(a);
    return a.add({ kind: "call", func: f, args: [], keywords: k });
  };
  const method = (recv: Lit, attr: string) => (a: Ast) => {
    const r = a.lit(recv);
    const f = a.add({ kind: "attribute", value: r, attr });
    const k = kw(a);
    return a.add({ kind: "call", func: f, args: [], keywords: k });
  };
  for (const fn of ["dict", "min", "max", "round", "sum", "int", "str", "len", "abs", "float", "bool", "repr", "list", "tuple", "ascii"]) {
    dup(builtin(fn), fn);
  }
  for (const attr of ["format", "join", "replace", "startswith", "strip", "lower"]) {
    dup(method("x", attr), `str.${attr}`);
  }
  dup(method({ d: [] }, "get"), "dict.get");
  dup(method({ d: [] }, "keys"), "dict.keys");
  // Two ** mappings with the same key.
  expect(pyError(() => m("{a}", "format", [], [[null, { d: [["a", 1n]] }], [null, { d: [["a", 2n]] }]]))).toEqual({
    cls: "TypeError",
    msg: "str.format() got multiple values for keyword argument 'a'",
  });
});

/// (partial: * and ** unpacking errors name the callable like CPython 3.12)
test("a bad * or ** operand names the callable", () => {
  const unpack = (callee: (a: Ast) => number, starred: boolean, operand: Lit) => (a: Ast) => {
    const f = callee(a);
    const v = a.lit(operand);
    if (starred) {
      const st = a.add({ kind: "starred", value: v });
      return a.add({ kind: "call", func: f, args: [st], keywords: [] });
    }
    return a.add({ kind: "call", func: f, args: [], keywords: [{ name: null, value: v }] });
  };
  const builtin = (fn: string) => (a: Ast) => a.name(fn);
  const method = (recv: Lit, attr: string) => (a: Ast) => {
    const r = a.lit(recv);
    return a.add({ kind: "attribute", value: r, attr });
  };
  const cases: [(a: Ast) => number, boolean, Lit, string][] = [
    [builtin("min"), true, 1n, "min() argument after * must be an iterable, not int"],
    [builtin("dict"), false, 1n, "dict() argument after ** must be a mapping, not int"],
    [method("x", "format"), false, { l: [1n] }, "str.format() argument after ** must be a mapping, not list"],
    [method("x", "format"), true, null, "str.format() argument after * must be an iterable, not NoneType"],
    [method({ d: [] }, "get"), true, 1n, "dict.get() argument after * must be an iterable, not int"],
    [method({ d: [] }, "items"), false, 1n, "dict.items() argument after ** must be a mapping, not int"],
  ];
  for (const [callee, starred, operand, msg] of cases) {
    expect(pyError(() => run(unpack(callee, starred, operand)))).toEqual({ cls: "TypeError", msg });
  }
});

test("starred call arguments unpack supported iterables in order", () => {
  const capture = callableVal("capture", (args) => listVal(args));
  const ast = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "capture" },
      { kind: "literal", value: { kind: "str", value: "aé" } },
      { kind: "starred", value: 1 },
      { kind: "literal", value: { kind: "bytes", value: [2, 255] } },
      { kind: "starred", value: 3 },
      { kind: "literal", value: { kind: "str", value: "key" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "str", value: "second" } },
      { kind: "literal", value: { kind: "int", value: "2" } },
      { kind: "dict", items: [[5, 6], [7, 8]] },
      { kind: "starred", value: 9 },
      { kind: "literal", value: { kind: "int", value: "3" } },
      { kind: "literal", value: { kind: "int", value: "4" } },
      { kind: "list", items: [12] },
      { kind: "starred", value: 13 },
      { kind: "literal", value: { kind: "str", value: "z" } },
      { kind: "tuple", items: [15] },
      { kind: "starred", value: 16 },
      { kind: "call", func: 0, args: [2, 4, 10, 11, 14, 17], keywords: [] },
    ],
  });
  expect(evalAst(ast, createContext({ capture }))).toEqual(listVal([
    strVal("a"), strVal("é"), intVal(2n), intVal(255n), strVal("key"), strVal("second"), intVal(3n), intVal(4n), strVal("z"),
  ]));
});

test("call arguments are evaluated before noncallability is reported", () => {
  const ast = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "name", name: "len" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "call", func: 1, args: [2], keywords: [] },
      { kind: "call", func: 0, args: [3], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(ast, createContext()))).toEqual({
    cls: "TypeError",
    msg: "object of type 'int' has no len()",
  });

  const duplicate = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "str", value: "a" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "dict", items: [[1, 2]] },
      { kind: "literal", value: { kind: "int", value: "2" } },
      { kind: "call", func: 0, args: [], keywords: [{ name: null, value: 3 }, { name: "a", value: 4 }] },
    ],
  });
  expect(pyError(() => evalAst(duplicate, createContext()))).toEqual({
    cls: "TypeError",
    msg: "1 got multiple values for keyword argument 'a'",
  });

  const badStar = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "starred", value: 1 },
      { kind: "call", func: 0, args: [2], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(badStar, createContext()))).toEqual({
    cls: "TypeError",
    msg: "1 argument after * must be an iterable, not int",
  });

  const badDoubleStar = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "call", func: 0, args: [], keywords: [{ name: null, value: 1 }] },
    ],
  });
  expect(pyError(() => evalAst(badDoubleStar, createContext()))).toEqual({
    cls: "TypeError",
    msg: "1 argument after ** must be a mapping, not int",
  });

  const successfulCall = decode({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "literal", value: { kind: "str", value: "é" } },
      { kind: "starred", value: 1 },
      { kind: "call", func: 0, args: [2], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(successfulCall, createContext()))).toEqual({
    cls: "TypeError",
    msg: "'int' object is not callable",
  });

  const host = objectVal({
    typeName: () => "sample",
    getAttr: () => NONE,
    repr: () => "<sample>",
  });
  const hostBadStar = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "target" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "starred", value: 1 },
      { kind: "call", func: 0, args: [2], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(hostBadStar, createContext({ target: host })))).toEqual({
    cls: "TypeError",
    msg: "<sample> argument after * must be an iterable, not int",
  });
});

/// (partial: call argument accumulation obeys the sticky per-evaluation byte budget)
test("bounded starred argument accumulation refuses before invoking the callable", () => {
  let invoked = false;
  const capture = callableVal("capture", (args) => {
    invoked = true;
    return listVal(args);
  });
  const ast = decode({
    schema: 1,
    nodes: [
      { kind: "name", name: "capture" },
      { kind: "literal", value: { kind: "str", value: "abcdefghijklmnopqrstuvwx" } },
      { kind: "starred", value: 1 },
      { kind: "call", func: 0, args: [2], keywords: [] },
    ],
  });
  expect(pyError(() => evalAst(ast, createContext({ capture }, { maxTotalBytes: 256 })))).toEqual({
    cls: "LimitExceeded",
    msg: "evaluation memory budget of 256 bytes exceeded",
  });
  expect(invoked).toBe(false);
});

/// (partial: dict.get, keys, values and items argument errors)
test("dict methods refuse arguments they do not take, with CPython 3.12 messages", () => {
  for (const attr of ["keys", "values", "items"]) {
    expect(pyError(() => m({ d: [] }, attr, [], [["x", 1n]]))).toEqual({
      cls: "TypeError",
      msg: `dict.${attr}() takes no keyword arguments`,
    });
    for (const args of [[1n], [1n, 2n]]) {
      expect(pyError(() => m({ d: [] }, attr, args))).toEqual({
        cls: "TypeError",
        msg: `dict.${attr}() takes no arguments (${args.length} given)`,
      });
    }
  }
  // dict.get is positional-only: `default=` is refused like any other keyword.
  for (const kw of ["default", "foo"]) {
    expect(pyError(() => m({ d: [] }, "get", [1n], [[kw, 2n]]))).toEqual({
      cls: "TypeError",
      msg: "dict.get() takes no keyword arguments",
    });
  }
  expect(pyError(() => m({ d: [] }, "get", []))).toEqual({
    cls: "TypeError",
    msg: "get expected at least 1 argument, got 0",
  });
  expect(pyError(() => m({ d: [] }, "get", [1n, 2n, 3n]))).toEqual({
    cls: "TypeError",
    msg: "get expected at most 2 arguments, got 3",
  });
  expect(m({ d: [["a", 1n]] }, "get", ["b", 2n])).toEqual(intVal(2n));
  expect(m({ d: [["a", 1n]] }, "get", ["b"])).toEqual(NONE);
});

/// (partial: repr and ascii argument count and keyword errors)
test("repr and ascii argument errors use CPython 3.12 messages", () => {
  for (const fn of ["repr", "ascii"]) {
    for (const n of [0, 2]) {
      const args = n === 0 ? [] : [1n, 2n];
      expect(
        pyError(() =>
          run((a) => {
            const items = args.map((x) => a.lit(x));
            a.call(fn, items);
          }),
        ),
      ).toEqual({ cls: "TypeError", msg: `${fn}() takes exactly one argument (${n} given)` });
    }
    expect(
      pyError(() =>
        run((a) => {
          const f = a.name(fn);
          const v = a.lit(1n);
          const k = a.lit(1n);
          a.add({ kind: "call", func: f, args: [v], keywords: [{ name: "x", value: k }] });
        }),
      ),
    ).toEqual({ cls: "TypeError", msg: `${fn}() takes no keyword arguments` });
  }
});

/// (partial: lower, upper and capitalize argument errors)
test("lower, upper and capitalize argument errors use CPython 3.12 messages", () => {
  for (const attr of ["lower", "upper", "capitalize"]) {
    expect(pyError(() => m("a", attr, [], [["x", 1n]]))).toEqual({
      cls: "TypeError",
      msg: `str.${attr}() takes no keyword arguments`,
    });
    for (const args of [[1n], [1n, 2n]]) {
      expect(pyError(() => m("a", attr, args))).toEqual({
        cls: "TypeError",
        msg: `str.${attr}() takes no arguments (${args.length} given)`,
      });
    }
  }
  for (const attr of ["strip", "lstrip", "rstrip"]) {
    expect(pyError(() => m("a", attr, [], [["x", 1n]]))).toEqual({
      cls: "TypeError",
      msg: `str.${attr}() takes no keyword arguments`,
    });
    expect(pyError(() => m("a", attr, [1n, 2n]))).toEqual({
      cls: "TypeError",
      msg: `${attr} expected at most 1 argument, got 2`,
    });
    expect(pyError(() => m("a", attr, [1n]))).toEqual({
      cls: "TypeError",
      msg: `${attr} arg must be None or str`,
    });
  }
});

/// (partial: other kwargs consumers ignore JS prototype names)
test("builtin keyword consumers use own keys only", () => {
  // dict(**{"__proto__": 1}) is a one-entry dict whose key is the string.
  const d = run((a) => {
    const f = a.name("dict");
    const v = a.lit({ d: [["__proto__", 1n]] });
    a.add({ kind: "call", func: f, args: [], keywords: [{ name: null, value: v }] });
  });
  expect(repr(d)).toBe("{'__proto__': 1}");
});

/// (partial: f/e/% round the exact binary value half to even)
test("fixed and exponent formatting round the exact binary value, half to even", () => {
  expect(str(pct("%.0f", 0.5))).toBe("0");
  expect(str(pct("%.0f", 2.5))).toBe("2");
  expect(str(pct("%.0f", 1.5))).toBe("2");
  expect(str(pct("%.1f", 0.25))).toBe("0.2");
  expect(str(pct("%.2f", 2.675))).toBe("2.67");
  expect(str(pct("%.2f", 1.005))).toBe("1.00");
  expect(str(pct("%.1f", 9.95))).toBe("9.9");
  expect(str(pct("%.1f", 0.05))).toBe("0.1");
  expect(str(pct("%.0f", 0.49999999999999994))).toBe("0");
  expect(str(pct("%.0f", -0.5))).toBe("-0");
  expect(str(fs(0.125, ".2f"))).toBe("0.12");
  expect(str(fs(0.375, ".2f"))).toBe("0.38");
  expect(str(pct("%.0e", 2.5))).toBe("2e+00");
  expect(str(pct("%.0e", 0.5))).toBe("5e-01");
  expect(str(pct("%.2e", 1.005))).toBe("1.00e+00");
  expect(str(fs(9.96, ".1e"))).toBe("1.0e+01");
  expect(str(fs(9.5, ".0e"))).toBe("1e+01");
  expect(str(fs(8.5, ".0e"))).toBe("8e+00");
  expect(str(fs(0.005, ".0%"))).toBe("0%");
  expect(str(fs(0.025, ".0%"))).toBe("2%");
  expect(str(fs(0.125, ".0%"))).toBe("12%");
  expect(str(fs(5e-324, "e"))).toBe("4.940656e-324");
  expect(str(fs(5e-324, ".20e"))).toBe("4.94065645841246544177e-324");
  expect(str(fs(5e-324, ".0e"))).toBe("5e-324");
  expect(str(fs(5e-324, "f"))).toBe("0.000000");
  expect(str(fs(-0.0, "e"))).toBe("-0.000000e+00");
  expect(str(pct("%e", 0.0))).toBe("0.000000e+00");
  expect(str(pct("%5.0f|%-6.1e|%+g", { t: [2.5, 0.25, 0.5] }))).toBe("    2|2.5e-01|+0.5");
  expect(str(fs(1234567.125, ",.2f"))).toBe("1,234,567.12");
  expect(str(fs(-1234.5, ",.0f"))).toBe("-1,234");
  expect(str(fs(-1.005, "010.2f"))).toBe("-000001.00");
});

/// (partial: g, n and no-type precision formatting from exact digits)
test("general and no-type formatting choose notation from the rounded exponent", () => {
  expect(str(pct("%.0g", 2.5))).toBe("2");
  expect(str(pct("%.1g", 0.25))).toBe("0.2");
  expect(str(pct("%.3g", 1e21))).toBe("1e+21");
  expect(str(pct("%g", 2.5e-5))).toBe("2.5e-05");
  expect(str(pct("%g", 1e-5))).toBe("1e-05");
  expect(str(pct("%g", 123456789.0))).toBe("1.23457e+08");
  expect(str(pct("%.3g", 0.0001234))).toBe("0.000123");
  expect(str(pct("%.20g", 0.1))).toBe("0.10000000000000000555");
  expect(str(pct("%.17g", 0.1))).toBe("0.10000000000000001");
  expect(str(pct("%.16g", 0.1))).toBe("0.1");
  expect(str(fs(999999.5, "g"))).toBe("1e+06");
  expect(str(fs(9999995.0, "g"))).toBe("1e+07");
  expect(str(fs(9.5, ".0g"))).toBe("1e+01");
  expect(str(fs(0.0001, "g"))).toBe("0.0001");
  expect(str(fs(1e16, "g"))).toBe("1e+16");
  expect(str(fs(-0.0, "g"))).toBe("-0");
  expect(str(fs(1.7976931348623157e308, "g"))).toBe("1.79769e+308");
  expect(str(fs(2.5, ".0n"))).toBe("2");
  expect(str(fs(1e21, ",g"))).toBe("1e+21");
  expect(str(fs(2.675, ".2"))).toBe("2.7");
  expect(str(fs(1e21, ".3"))).toBe("1e+21");
  expect(str(fs(0.25, ".1"))).toBe("0.2");
  expect(str(fs(1234.5, ".3"))).toBe("1.23e+03");
  expect(str(fs(0.5, ".3"))).toBe("0.5");
  expect(str(fs(100.0, ".3"))).toBe("1e+02");
  expect(str(fs(2.5, ".0"))).toBe("2e+00");
  expect(str(fs(150.0, ".1"))).toBe("2e+02");
  expect(str(fs(0.0, ".1"))).toBe("0e+00");
  expect(str(fs(0.0, ".2"))).toBe("0.0");
  expect(str(fs(-0.0, ".3"))).toBe("-0.0");
  expect(str(fs(1234567.0, "_.3"))).toBe("1.23e+06");
});

/// (partial: alternate form and z coercion on float formats)
test("alternate form keeps the decimal point and z coerces a rounded negative zero", () => {
  expect(str(pct("%#.0f", 1.0))).toBe("1.");
  expect(str(pct("%#.0e", 1.0))).toBe("1.e+00");
  expect(str(pct("%#g", 1.0))).toBe("1.00000");
  expect(str(pct("%#.3g", 0.0001))).toBe("0.000100");
  expect(str(fs(1.0, "#.0g"))).toBe("1.");
  expect(str(fs(1e-5, "#g"))).toBe("1.00000e-05");
  expect(str(fs(1e21, "#.3"))).toBe("1.00e+21");
  expect(str(fs(1.0, "#.3"))).toBe("1.00");
  expect(str(fs(0.5, "#.0%"))).toBe("50.%");
  expect(str(fs(0.0, "#.0e"))).toBe("0.e+00");
  expect(str(fs(0.0, "#.0f"))).toBe("0.");
  expect(str(fs(0.0, "#.0g"))).toBe("0.");
  expect(str(fs(1e21, "#"))).toBe("1.e+21");
  expect(str(fs(1.0, "#"))).toBe("1.0");
  expect(str(fs(-0.4, "z.0f"))).toBe("0");
  expect(str(fs(-0.04, "z.1f"))).toBe("0.0");
  expect(str(fs(-0.06, "z.1f"))).toBe("-0.1");
  expect(str(fs(-0.04, ".1f"))).toBe("-0.0");
  expect(str(fs(-0.0, ".0f"))).toBe("-0");
});

/// (partial: f format of values at or above 1e21 prints every digit)
test("fixed formatting of values at or above 1e21 prints full digits", () => {
  expect(str(pct("%.0f", 1e21))).toBe("1000000000000000000000");
  expect(str(pct("%.0f", 1e22))).toBe("10000000000000000000000");
  expect(str(pct("%.0f", -1e21))).toBe("-1000000000000000000000");
  expect(str(fs(1e21, ",.1f"))).toBe("1,000,000,000,000,000,000,000.0");
  expect(str(fs(1e20, "%"))).toBe("10000000000000000000000.000000%");
  expect(str(fs(1e307, "%"))).toBe("inf%");
  expect(str(fs(Infinity, ","))).toBe("inf");
  expect(str(pct("%f", 1e300))).toBe(
    "1000000000000000052504760255204420248704468581108159154915854115511802457988908195786371375080447864043704443832883878176942523235360430575644792184786706982848387200926575803737830233794788090059368953234970799945081119038967640880074652742780142494579258788820056842838115669472196386865459400540160.000000",
  );
  expect(str(fs(1.7976931348623157e308, ".0f"))).toBe(
    "179769313486231570814527423731704356798070567525844996598917476803157260780028538760589558632766878171540458953514382464234321326889464182768467546703537516986049910576551282076245490090389328944075868508455133942304583236903222948165808559332123348274797826204144723168738177180919299881250404026184124858368",
  );
  expect(str(pct("%.0f", 9223372036854775807n))).toBe("9223372036854775808");
  expect(str(pct("%.3f", 100000000000000000000.0))).toBe("100000000000000000000.000");
});

/// (partial: precision up to maxFormatWidth produces exact digits, never a native RangeError)
test("precision up to the format limit yields exact digits and above it a named limit error", () => {
  expect(str(pct("%.101f", 0.1))).toBe(
    "0.10000000000000000555111512312578270211815834045410156250000000000000000000000000000000000000000000000",
  );
  expect(str(fs(0.1, ".30f"))).toBe("0.100000000000000005551115123126");
  const f1000 = str(pct("%.1000f", 0.1));
  expect(f1000.length).toBe(1002);
  expect(f1000.startsWith("0.1000000000000000055511151231257827021181583404541015625000")).toBe(true);
  expect(/^0\.1000000000000000055511151231257827021181583404541015625(0+)$/.test(f1000)).toBe(true);
  const e1000 = str(fs(0.1, ".1000e"));
  expect(e1000.length).toBe(1006);
  expect(e1000.endsWith("0e-01")).toBe(true);
  expect(str(fs(0.1, ".1000g"))).toBe("0.1000000000000000055511151231257827021181583404541015625");
  expect(pyError(() => pct("%.1001f", 0.1))).toEqual({
    cls: "LimitExceeded",
    msg: "format width or precision 1001 exceeds the limit of 1000",
  });
  expect(pyError(() => fs(0.1, ".1001e"))).toEqual({
    cls: "LimitExceeded",
    msg: "format width or precision 1001 exceeds the limit of 1000",
  });
});

// Low-level formatter guards remain independently tested with out-of-domain values. The host
// entrypoint now rejects these integers as i64 overflow before any formatting operation.
test("low-level formatting guards huge integers and host admission enforces i64 first", () => {
  const huge = intVal(10n ** 400n);
  const edge = intVal(2n ** 1024n - 2n ** 970n);
  for (const spec of ["f", "e", "g", "%", ".0f"]) {
    expect(pyError(() => formatValue(huge, spec, new Budget()))).toEqual({
      cls: "OverflowError", msg: "int too large to convert to float",
    });
    expect(pyError(() => fsName({ v: huge }, spec))).toEqual({ cls: "OverflowError", msg: "integer overflow" });
  }
  expect(pyError(() => formatValue(edge, "f", new Budget()))).toEqual({ cls: "OverflowError", msg: "int too large to convert to float" });
  for (const conv of ["%f", "%e", "%g", "%.0f"]) {
    expect(pyError(() => printf(conv, huge, new Budget()))).toEqual({ cls: "OverflowError", msg: "int too large to convert to float" });
  }
  expect(str(fsName({ v: intVal(2n ** 53n + 1n) }, "f"))).toBe("9007199254740992.000000");
  expect(formatValue(intVal(2n ** 64n + 1n), ".0f", new Budget())).toBe("18446744073709551616");
  expect(pyError(() => intToFloat(huge.value))).toEqual({ cls: "OverflowError", msg: "int too large to convert to float" });
  expect(pyError(() => run((a) => a.call("float", [a.name("v")]), { v: huge }))).toEqual({
    cls: "OverflowError", msg: "integer overflow",
  });
});

/// (partial: integrated date-family str, f-string, str.format and percent formatting)
test("DATES values use the bounded FORMAT conversion pipeline", () => {
  const values: [ExprValue, string][] = [
    [{ kind: "date", year: 2020, month: 1, day: 1 }, "2020-01-01"],
    [{ kind: "datetime", year: 2020, month: 1, day: 1, hour: 2, minute: 3, second: 4, microsecond: 5 }, "2020-01-01 02:03:04.000005"],
    [{ kind: "timedelta", days: 1, seconds: 2, microseconds: 3 }, "1 day, 0:00:02.000003"],
    [relativeDeltaVal({ days: 1n }), "relativedelta(days=+1)"],
  ];
  for (const [v, expected] of values) {
    expect(str(run((a) => a.call("str", [a.name("v")]), { v }))).toBe(expected);
    expect(str(fsName({ v }, null))).toBe(expected);
    expect(str(fsName({ v }, null, "s"))).toBe(expected);
    expect(str(fsName({ v }, ">40", "s"))).toBe(expected.padStart(40));
    expect(str(fsName({ v }, null, "r"))).toBe(repr(v));
    expect(str(run((a) => {
      const t = a.lit("{}");
      const f = a.add({ kind: "attribute", value: t, attr: "format" });
      const arg = a.name("v");
      a.add({ kind: "call", func: f, args: [arg], keywords: [] });
    }, { v }))).toBe(expected);
    expect(str(run((a) => {
      const l = a.lit("%s");
      const r = a.name("v");
      a.add({ kind: "binary", op: "mod", left: l, right: r });
    }, { v }))).toBe(expected);
    if (v.kind === "date" || v.kind === "datetime") {
      expect(str(fsName({ v }, "%Y-%m-%d"))).toBe("2020-01-01");
    } else {
      const type = v.kind === "timedelta" ? "datetime.timedelta" : "relativedelta";
      expect(pyError(() => fsName({ v }, ">20"))).toEqual({
        cls: "TypeError", msg: `unsupported format string passed to ${type}.__format__`,
      });
    }
  }
});

/// (partial: repr of timedelta and integrated relativedelta)
test("repr of delta values preserves normalized fields", () => {
  const td = (days: number, seconds: number, microseconds: number): ExprValue => ({ kind: "timedelta", days, seconds, microseconds });
  const r = (v: ExprValue) => str(run((a) => a.call("repr", [a.name("v")]), { v }));
  expect(r(td(1, 0, 0))).toBe("datetime.timedelta(days=1)");
  expect(r(td(0, 5, 0))).toBe("datetime.timedelta(seconds=5)");
  expect(r(td(-1, 5, 3))).toBe("datetime.timedelta(days=-1, seconds=5, microseconds=3)");
  expect(r(td(0, 0, 0))).toBe("datetime.timedelta(0)");
  expect(r(relativeDeltaVal({ days: 1n }))).toBe("relativedelta(days=+1)");
});

/// (partial: startswith/endswith start and end with slice normalisation)
test("startswith and endswith honour start and end like a slice", () => {
  const b = (v: ExprValue) => {
    if (v.kind !== "bool") throw new Error(`expected bool, got ${v.kind}`);
    return v.value;
  };
  expect(b(m("abc", "startswith", ["b", 1n]))).toBe(true);
  expect(b(m("abc", "startswith", ["b", 1n, 2n]))).toBe(true);
  expect(b(m("abc", "startswith", ["bc", 1n, 2n]))).toBe(false);
  expect(b(m("abc", "startswith", ["a", -3n]))).toBe(true);
  expect(b(m("abc", "startswith", ["", 3n]))).toBe(true);
  expect(b(m("abc", "startswith", ["", 4n]))).toBe(false);
  expect(b(m("abc", "startswith", ["", 5n, 2n]))).toBe(false);
  expect(b(m("abc", "startswith", ["a", true]))).toBe(false);
  expect(b(m("abc", "startswith", ["a", null, null]))).toBe(true);
  expect(b(m(" a ", "startswith", [" ", 0n, null]))).toBe(true);
  expect(b(m("abc", "startswith", ["a", 9223372036854775807n]))).toBe(false);
  expect(b(m("abc", "startswith", ["", -9223372036854775807n, 9223372036854775807n]))).toBe(true);
  expect(b(m("abc", "endswith", ["b", null, 2n]))).toBe(true);
  expect(b(m("abc", "endswith", ["b", null, -1n]))).toBe(true);
  expect(b(m("abc", "endswith", ["c", -1n]))).toBe(true);
  expect(b(m("abc", "endswith", ["ab", 0n, -1n]))).toBe(true);
  expect(b(m("abc", "endswith", ["", 10n, 20n]))).toBe(false);
  expect(b(m("abc", "endswith", ["", 2n, 1n]))).toBe(false);
  // Code points, not UTF-16 units.
  expect(b(m("😀ab", "startswith", ["a", 1n]))).toBe(true);
  expect(b(m("a😀b", "endswith", ["😀", 0n, 2n]))).toBe(true);
});

/// (partial: startswith/endswith tuple prefixes with start and end, lazy item checks)
test("startswith and endswith accept a tuple of prefixes and check items in order", () => {
  expect(m("abc", "startswith", [{ t: ["x", "b"] }, 1n])).toEqual(boolVal(true));
  expect(m("abc", "startswith", [{ t: [] }, 0n])).toEqual(boolVal(false));
  expect(m("abc", "startswith", [{ t: ["a", 1n] }])).toEqual(boolVal(true));
  expect(pyError(() => m("abc", "startswith", [{ t: ["x", 1n] }]))).toEqual({
    cls: "TypeError",
    msg: "tuple for startswith must only contain str, not int",
  });
  expect(pyError(() => m("abc", "endswith", [{ t: ["x", 1n] }]))).toEqual({
    cls: "TypeError",
    msg: "tuple for endswith must only contain str, not int",
  });
});

/// (partial: startswith/endswith argument errors)
test("startswith and endswith argument errors use CPython 3.12 messages", () => {
  const slice = { cls: "TypeError", msg: "slice indices must be integers or None or have an __index__ method" };
  expect(pyError(() => m("abc", "startswith", ["a", "1"]))).toEqual(slice);
  expect(pyError(() => m("abc", "startswith", ["a", 1.0]))).toEqual(slice);
  expect(pyError(() => m("abc", "startswith", ["a", { l: [] }]))).toEqual(slice);
  expect(pyError(() => m("abc", "endswith", ["a", 0n, "x"]))).toEqual(slice);
  expect(pyError(() => m("abc", "startswith", [1n, "x"]))).toEqual(slice);
  expect(pyError(() => m("abc", "startswith", [], [["prefix", "a"]]))).toEqual({
    cls: "TypeError",
    msg: "str.startswith() takes no keyword arguments",
  });
  expect(pyError(() => m("abc", "startswith", ["a", 0n, 1n, 2n]))).toEqual({
    cls: "TypeError",
    msg: "startswith() takes at most 3 arguments (4 given)",
  });
  expect(pyError(() => m("abc", "startswith", []))).toEqual({
    cls: "TypeError",
    msg: "startswith() takes at least 1 argument (0 given)",
  });
  expect(pyError(() => m("abc", "startswith", [{ l: [1n] }]))).toEqual({
    cls: "TypeError",
    msg: "startswith first arg must be str or a tuple of str, not list",
  });
});

/// (partial: split binds sep and maxsplit by position and name)
test("split binds sep and maxsplit positionally and by keyword", () => {
  const strs = (v: ExprValue) => {
    if (v.kind !== "list") throw new Error(`expected list, got ${v.kind}`);
    return v.items.map(str);
  };
  expect(strs(m("a b,c", "split", [], [["sep", null]]))).toEqual(["a", "b,c"]);
  expect(strs(m("a  b c", "split", [], [["maxsplit", 1n]]))).toEqual(["a", "b c"]);
  expect(strs(m("a b", "split", [null], [["maxsplit", 1n]]))).toEqual(["a", "b"]);
  expect(strs(m("a b", "split", [], [["maxsplit", 1n], ["sep", null]]))).toEqual(["a", "b"]);
  expect(strs(m("a,b", "split", [","], [["maxsplit", 1n]]))).toEqual(["a", "b"]);
  expect(strs(m("a,b,c", "split", [",", -1n]))).toEqual(["a", "b", "c"]);
  expect(strs(m("a,b,c", "split", [",", true]))).toEqual(["a", "b,c"]);
  expect(strs(m("a b c", "split", [null, 0n]))).toEqual(["a b c"]);
  expect(strs(m("  a b c", "split", [null, 0n]))).toEqual(["a b c"]);
  expect(strs(m("  ", "split", [null, 0n]))).toEqual([]);
  expect(strs(m("a b  ", "split", [null, 1n]))).toEqual(["a", "b  "]);
  expect(strs(m("a b  ", "split", [null, 2n]))).toEqual(["a", "b"]);
  expect(strs(m(" a b ", "split", [null, 5n]))).toEqual(["a", "b"]);
  expect(strs(m("a,b", "split", [",", 5n]))).toEqual(["a", "b"]);
  expect(strs(m("a,b,", "split", [",", 1n]))).toEqual(["a", "b,"]);
});

/// (partial: split rejects unknown, duplicate and surplus arguments)
test("split argument errors use CPython 3.12 messages", () => {
  const e = (args: Lit[], kw: [string | null, Lit][] = []) => pyError(() => m("a b", "split", args, kw));
  expect(e([], [["foo", 1n]])).toEqual({ cls: "TypeError", msg: "'foo' is an invalid keyword argument for split()" });
  expect(e([], [["sep", null], ["foo", 2n]])).toEqual({ cls: "TypeError", msg: "'foo' is an invalid keyword argument for split()" });
  expect(e([], [["sep", 1n], ["foo", 2n]])).toEqual({ cls: "TypeError", msg: "'foo' is an invalid keyword argument for split()" });
  expect(e([], [["maxsplit", 1n], ["maxsplit2", 1n]])).toEqual({ cls: "TypeError", msg: "'maxsplit2' is an invalid keyword argument for split()" });
  expect(e([","], [["sep", ","]])).toEqual({ cls: "TypeError", msg: "argument for split() given by name ('sep') and position (1)" });
  expect(e([","], [["sep", null]])).toEqual({ cls: "TypeError", msg: "argument for split() given by name ('sep') and position (1)" });
  expect(e([1n], [["sep", 2n]])).toEqual({ cls: "TypeError", msg: "argument for split() given by name ('sep') and position (1)" });
  expect(e([",", 1n], [["maxsplit", 1n]])).toEqual({ cls: "TypeError", msg: "split() takes at most 2 arguments (3 given)" });
  expect(e([null, 1n, 2n])).toEqual({ cls: "TypeError", msg: "split() takes at most 2 arguments (3 given)" });
  expect(e([null, 1n], [["sep", null]])).toEqual({ cls: "TypeError", msg: "split() takes at most 2 arguments (3 given)" });
  expect(e([], [["maxsplit", "x"]])).toEqual({ cls: "TypeError", msg: "'str' object cannot be interpreted as an integer" });
  expect(e([null, null])).toEqual({ cls: "TypeError", msg: "'NoneType' object cannot be interpreted as an integer" });
  expect(e([null, 1.0])).toEqual({ cls: "TypeError", msg: "'float' object cannot be interpreted as an integer" });
  expect(e([1n])).toEqual({ cls: "TypeError", msg: "must be str or None, not int" });
  expect(e([], [["sep", 1n]])).toEqual({ cls: "TypeError", msg: "must be str or None, not int" });
  expect(e([""])).toEqual({ cls: "ValueError", msg: "empty separator" });
  expect(e([], [["sep", ""]])).toEqual({ cls: "ValueError", msg: "empty separator" });
});

/// (partial: printf integer conversions of NaN and infinity)
test("printf %d, %i and %u of NaN raise ValueError and of infinity OverflowError", () => {
  for (const conv of ["%d", "%i", "%u", "%5d"]) {
    expect(pyError(() => pct(conv, NaN))).toEqual({ cls: "ValueError", msg: "cannot convert float NaN to integer" });
    expect(pyError(() => pct(conv, Infinity))).toEqual({ cls: "OverflowError", msg: "cannot convert float infinity to integer" });
    expect(pyError(() => pct(conv, -Infinity))).toEqual({ cls: "OverflowError", msg: "cannot convert float infinity to integer" });
  }
  expect(pyError(() => fs(NaN, "d"))).toEqual({ cls: "ValueError", msg: "Unknown format code 'd' for object of type 'float'" });
  expect(pyError(() => pct("%x", NaN))).toEqual({ cls: "TypeError", msg: "%x format: an integer is required, not float" });
});

/// (partial: str.join over str characters and dict keys)
test("str.join iterates a str's characters and a dict's keys in insertion order", () => {
  expect(str(m("-", "join", ["ab"]))).toBe("a-b");
  expect(str(m("-", "join", [""]))).toBe("");
  expect(str(m("-", "join", [{ d: [["x", 1n], ["y", 2n]] }]))).toBe("x-y");
  expect(str(m("", "join", [{ d: [] }]))).toBe("");
  expect(pyError(() => m("-", "join", [{ d: [[1n, 2n]] }]))).toEqual({
    cls: "TypeError",
    msg: "sequence item 0: expected str instance, int found",
  });
  expect(pyError(() => m("-", "join", [{ d: [["a", 1n], [2n, 3n]] }]))).toEqual({
    cls: "TypeError",
    msg: "sequence item 1: expected str instance, int found",
  });
  expect(pyError(() => m("-", "join", [{ t: ["a", null] }]))).toEqual({
    cls: "TypeError",
    msg: "sequence item 1: expected str instance, NoneType found",
  });
  for (const bad of [5n, 1.5, null]) {
    expect(pyError(() => m("-", "join", [bad]))).toEqual({ cls: "TypeError", msg: "can only join an iterable" });
  }
  expect(pyError(() => m("-", "join", [{ l: ["a"] }, 1n]))).toEqual({
    cls: "TypeError",
    msg: "str.join() takes exactly one argument (2 given)",
  });
  expect(pyError(() => m("-", "join", []))).toEqual({ cls: "TypeError", msg: "str.join() takes exactly one argument (0 given)" });
  expect(pyError(() => m("-", "join", [], [["iterable", { l: ["a"] }]]))).toEqual({
    cls: "TypeError",
    msg: "str.join() takes no keyword arguments",
  });
});

/// (partial: bytes are iterated as ints by str.join)
test("str.join over bytes reports the int items", () => {
  const v = bytesVal([0x61, 0x62]);
  expect(pyError(() => run((a) => {
    const s = a.lit("-");
    const f = a.add({ kind: "attribute", value: s, attr: "join" });
    a.add({ kind: "call", func: f, args: [a.name("v")], keywords: [] });
  }, { v }))).toEqual({ cls: "TypeError", msg: "sequence item 0: expected str instance, int found" });
});

/// (partial: split and strip use Python's whitespace set)
test("split and strip use Python's whitespace set, not JS \\s", () => {
  const strs = (v: ExprValue) => {
    if (v.kind !== "list") throw new Error(`expected list, got ${v.kind}`);
    return v.items.map(str);
  };
  expect(strs(m("a\x1cb\x85c\xa0d e", "split", []))).toEqual(["a", "b", "c", "d", "e"]);
  expect(strs(m(" a b c d e", "split", []))).toEqual(["a", "b", "c", "d", "e"]);
  expect(strs(m("\x0b\x0cx", "split", []))).toEqual(["x"]);
  expect(strs(m("﻿x", "split", []))).toEqual(["﻿x"]);
  expect(strs(m("a​b", "split", []))).toEqual(["a​b"]);
  expect(strs(m("a᠎b", "split", []))).toEqual(["a᠎b"]);
  expect(str(m("\x1c\x1d\x1e\x1f\x85\xa0 x 　", "strip", []))).toBe("x");
  expect(str(m("​x", "strip", []))).toBe("​x");
  expect(str(m("﻿x", "strip", []))).toBe("﻿x");
  expect(str(m("  x\x85", "rstrip", []))).toBe("  x");
  expect(str(m("\x85x", "lstrip", []))).toBe("x");
  expect(pyError(() => m("x", "strip", [], [["chars", "x"]]))).toEqual({
    cls: "TypeError",
    msg: "str.strip() takes no keyword arguments",
  });
});

/// (partial: capitalize titlecases the first character)
test("capitalize uses the titlecase of the first character", () => {
  expect(str(m("ǆabc", "capitalize", []))).toBe("ǅabc");
  expect(str(m("ßa", "capitalize", []))).toBe("Ssa");
  expect(str(m("ﬁx", "capitalize", []))).toBe("Fix");
  expect(str(m("ŉX", "capitalize", []))).toBe("ʼNx");
  expect(str(m("ᾳΑ", "capitalize", []))).toBe("ᾼα");
  expect(str(m("ᾷ", "capitalize", []))).toBe("ᾼ͂");
  expect(str(m("ა", "capitalize", []))).toBe("ა");
  expect(str(m("hELLO", "capitalize", []))).toBe("Hello");
  expect(str(m("ΑΣ", "capitalize", []))).toBe("Ας");
  expect(str(m("ΣΑΣ", "capitalize", []))).toBe("Σας");
  expect(str(m("aΣ b", "capitalize", []))).toBe("Aς b");
  expect(str(m("", "capitalize", []))).toBe("");
});

/// (partial: format spec combinations CPython rejects)
test("format specs reject grouping, sign, alternate form and z where CPython does", () => {
  const e = (v: Lit, spec: string) => pyError(() => fs(v, spec));
  expect(e(65n, ",c")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'c'." });
  expect(e(65n, "_c")).toEqual({ cls: "ValueError", msg: "Cannot specify '_' with 'c'." });
  expect(e(65n, "+,c")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'c'." });
  expect(e(65n, "z,c")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'c'." });
  for (const spec of ["+c", " c", "-c", "+#c"]) {
    expect(e(65n, spec)).toEqual({ cls: "ValueError", msg: "Sign not allowed with integer format specifier 'c'" });
  }
  expect(e(true, "+c")).toEqual({ cls: "ValueError", msg: "Sign not allowed with integer format specifier 'c'" });
  expect(e(65n, "#c")).toEqual({ cls: "ValueError", msg: "Alternate form (#) not allowed with integer format specifier 'c'" });
  expect(e(65n, "+.2c")).toEqual({ cls: "ValueError", msg: "Precision not allowed in integer format specifier" });
  expect(e(65n, "z.2d")).toEqual({ cls: "ValueError", msg: "Precision not allowed in integer format specifier" });
  for (const spec of ["zc", "zd", "+zc", "z#c"]) {
    expect(e(65n, spec)).toEqual({ cls: "ValueError", msg: "Negative zero coercion (z) not allowed in integer format specifier" });
  }
  expect(e("a", "z")).toEqual({ cls: "ValueError", msg: "Negative zero coercion (z) not allowed in string format specifier" });
  expect(e(65n, ",n")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'n'." });
  expect(e(65n, "_n")).toEqual({ cls: "ValueError", msg: "Cannot specify '_' with 'n'." });
  expect(e(6.5, ",n")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'n'." });
  expect(e(1.5, ",.2n")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'n'." });
  expect(e(1n, "z,n")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'n'." });
  expect(e(65n, ",b")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'b'." });
  expect(e(65n, ",x")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'x'." });
  expect(e(65n, ",o")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'o'." });
  expect(e("a", ",s")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 's'." });
  expect(e("a", ",")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 's'." });
  expect(e("a", "_")).toEqual({ cls: "ValueError", msg: "Cannot specify '_' with 's'." });
  expect(e(1n, ",q")).toEqual({ cls: "ValueError", msg: "Cannot specify ',' with 'q'." });
  expect(e(1n, "q")).toEqual({ cls: "ValueError", msg: "Unknown format code 'q' for object of type 'int'" });
  expect(e("a", "q")).toEqual({ cls: "ValueError", msg: "Unknown format code 'q' for object of type 'str'" });
  expect(e(1.0, "q")).toEqual({ cls: "ValueError", msg: "Unknown format code 'q' for object of type 'float'" });
  expect(e(65n, "z+c")).toEqual({ cls: "ValueError", msg: "Invalid format specifier 'z+c' for object of type 'int'" });
  // Accepted combinations.
  expect(str(fs(65n, "_b"))).toBe("100_0001");
  expect(str(fs(65535n, "_x"))).toBe("ffff");
  expect(str(fs(5n, "#_b"))).toBe("0b101");
  expect(str(fs(6.5, ",e"))).toBe("6.500000e+00");
  expect(str(fs(6.5, "_f"))).toBe("6.500000");
  expect(str(fs(12.5, ",%"))).toBe("1,250.000000%");
  expect(str(fs(1.0, "z"))).toBe("1.0");
  expect(str(fs(1n, "zf"))).toBe("1.000000");
  expect(str(fs(true, ","))).toBe("1");
  expect(str(fs(65n, "5c"))).toBe("    A");
  expect(str(fs(65n, "<5c"))).toBe("A    ");
  expect(str(fs(65n, "=5c"))).toBe("    A");
  expect(str(fs(65n, "05c"))).toBe("0000A");
});

/// (partial: recorded client gaps fail with an error naming the gap)
test("unsupported format features fail with errors that name the gap", () => {
  expect(pyError(() => m("{:{}}", "format", [1n, 3n]))).toEqual({
    cls: "ValueError",
    msg: "nested replacement fields in format specs are not supported",
  });
  expect(pyError(() => m("{0[0]}", "format", [{ l: [1n] }]))).toEqual({
    cls: "ValueError",
    msg: "format field accessors ('.' and '[') are not supported",
  });
  expect(pyError(() => m("{0.real}", "format", [1n]))).toEqual({
    cls: "ValueError",
    msg: "format field accessors ('.' and '[') are not supported",
  });
  const zeroGrouped = {
    cls: "NotImplementedError",
    msg: "zero padding combined with a grouping option is not supported by the client evaluator",
  };
  // CPython groups the padding whenever the effective fill is '0' and the alignment '='
  // (`00,001,234`); the client refuses those.
  for (const spec of ["010,", "0=10,", "=010,", "0=10_", "010_"]) {
    expect(pyError(() => fs(1234n, spec))).toEqual(zeroGrouped);
  }
  expect(pyError(() => fs(-1234n, "0=10,"))).toEqual(zeroGrouped);
  expect(pyError(() => fs(true, "0=10,"))).toEqual(zeroGrouped);
  expect(pyError(() => fs(65535n, "0=10_x"))).toEqual(zeroGrouped);
  expect(pyError(() => fs(5n, "010_b"))).toEqual(zeroGrouped);
  for (const spec of ["012,.1f", "0=12,.1f", "=012,.1f", "012_.1f"]) {
    expect(pyError(() => fs(1234.5, spec))).toEqual(zeroGrouped);
  }
  expect(pyError(() => fs(1.5, "0=10,"))).toEqual(zeroGrouped);
  // Also with a sign, an alternate-form prefix, and the e, g and % types once padding is needed.
  for (const [value, spec] of [
    [1234n, "+010,"],
    [1234n, "#012_x"],
    [1.5, "0=16,e"],
    [1.5, "016,e"],
    [1234n, "016,e"],
    [1234.5, "015,g"],
  ] as [Lit, string][]) {
    expect(pyError(() => fs(value, spec))).toEqual(zeroGrouped);
  }
  // inf and nan: CPython pads with plain zeros (no grouping), and so does the client.
  const inf = Number.POSITIVE_INFINITY;
  expect(str(fs(inf, "010,"))).toBe("0000000inf");
  expect(str(fs(inf, "0=10,"))).toBe("0000000inf");
  expect(str(fs(inf, "+010,"))).toBe("+000000inf");
  expect(str(fs(-inf, "010_"))).toBe("-000000inf");
  expect(str(fs(-inf, "0=10_"))).toBe("-000000inf");
  expect(str(fs(Number.NaN, "010,"))).toBe("0000000nan");
  expect(str(fs(Number.NaN, "+010,"))).toBe("+000000nan");
  expect(str(fs(1e308, "012,%"))).toBe("00000000inf%");
  // Other fills and alignments do not group the padding; CPython prints these.
  expect(str(fs(1234n, "0<10,"))).toBe("1,23400000");
  expect(str(fs(1234n, "0>10,"))).toBe("000001,234");
  expect(str(fs(1234n, "<010,"))).toBe("1,23400000");
  expect(str(fs(1234n, ">010,"))).toBe("000001,234");
  expect(str(fs(1234n, "^010,"))).toBe("001,234000");
  expect(str(fs(1234n, "x=10,"))).toBe("xxxxx1,234");
  expect(str(fs(1234n, "0=5,"))).toBe("1,234");
  expect(str(fs(1.5, "0=10,e"))).toBe("1.500000e+00");
  expect(str(fs(1234n, "010,e"))).toBe("1.234000e+03");
  expect(str(fs(0.5, "010,%"))).toBe("50.000000%");
  // No padding needed: the grouped value is printed.
  expect(str(fs(1234n, "05,"))).toBe("1,234");
  expect(str(fs(-1234567n, ","))).toBe("-1,234,567");
});

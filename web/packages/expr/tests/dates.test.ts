/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, it } from "vitest";
import {
  boolVal,
  createContext,
  decodeAst,
  evalAst,
  intVal,
  floatVal,
  strVal,
  parseBoundedJson,
  repr,
  type ValidatedAst,
  type DateTimeValue,
  type ExprValue,
  Budget,
  CommandNamespace,
  PythonError,
  SLOT,
  computeDeepBytes,
  computeDepth,
  computeNodes,
  dictVal,
  listVal,
  strftime,
} from "../src/index";

function decode(obj: unknown): ValidatedAst {
  return decodeAst(parseBoundedJson(JSON.stringify(obj)));
}

function fixedClock(): DateTimeValue {
  return {
    kind: "datetime",
    year: 2026,
    month: 10,
    day: 15,
    hour: 12,
    minute: 34,
    second: 56,
    microsecond: 0,
  };
}

describe("DATES client evaluator: Command, datetime, timedelta, relativedelta and clock", () => {
  it("evaluates Command namespace constants, methods and validation errors", () => {
    const ctx = createContext();

    // Command.CREATE => 0
    const astCreateConst = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "Command" },
        { kind: "attribute", value: 0, attr: "CREATE" },
      ],
    });
    expect(evalAst(astCreateConst, ctx)).toEqual(intVal(0n));

    // Command.SET => 6
    const astSetConst = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "Command" },
        { kind: "attribute", value: 0, attr: "SET" },
      ],
    });
    expect(evalAst(astSetConst, ctx)).toEqual(intVal(6n));

    // Command.create({'name': 'x'}) => (0, 0, {'name': 'x'})
    const astCreate = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "Command" },
        { kind: "attribute", value: 0, attr: "create" },
        { kind: "literal", value: { kind: "str", value: "name" } },
        { kind: "literal", value: { kind: "str", value: "x" } },
        { kind: "dict", items: [[2, 3]] },
        { kind: "call", func: 1, args: [4], keywords: [] },
      ],
    });
    expect(repr(evalAst(astCreate, ctx))).toBe("(0, 0, {'name': 'x'})");

    // Command.link() missing arg
    const astLinkMissing = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "Command" },
        { kind: "attribute", value: 0, attr: "link" },
        { kind: "call", func: 1, args: [], keywords: [] },
      ],
    });
    expect(() => evalAst(astLinkMissing, ctx)).toThrow(
      "TypeError: Command.link() missing 1 required positional argument: 'id'",
    );

    // Command.unknown
    const astUnknown = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "Command" },
        { kind: "attribute", value: 0, attr: "unknown" },
      ],
    });
    expect(() => evalAst(astUnknown, ctx)).toThrow(
      "AttributeError: type object 'Command' has no attribute 'unknown'",
    );
  });

  it("evaluates timedelta arithmetic, attributes and normalization", () => {
    const ctx = createContext();

    // timedelta(days=1, hours=2, minutes=3, seconds=4)
    const astTd = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "timedelta" },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "literal", value: { kind: "int", value: "2" } },
        { kind: "literal", value: { kind: "int", value: "3" } },
        { kind: "literal", value: { kind: "int", value: "4" } },
        {
          kind: "call",
          func: 0,
          args: [],
          keywords: [
            { name: "days", value: 1 },
            { name: "hours", value: 2 },
            { name: "minutes", value: 3 },
            { name: "seconds", value: 4 },
          ],
        },
      ],
    });
    const resTd = evalAst(astTd, ctx);
    expect(repr(resTd)).toBe("datetime.timedelta(days=1, seconds=7384)");
  });

  it("evaluates datetime construction, arithmetic and formatting", () => {
    const ctx = createContext();

    // datetime(2024, 1, 31)
    const astDt = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "datetime" },
        { kind: "literal", value: { kind: "int", value: "2024" } },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "literal", value: { kind: "int", value: "31" } },
        { kind: "call", func: 0, args: [1, 2, 3], keywords: [] },
      ],
    });
    const resDt = evalAst(astDt, ctx);
    expect(repr(resDt)).toBe("datetime.datetime(2024, 1, 31, 0, 0)");
  });

  it("enforces deterministic host clock contract", () => {
    // Without clock, datetime.now() fails loudly
    const ctxNoClock = createContext();
    const astNow = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "datetime" },
        { kind: "attribute", value: 0, attr: "now" },
        { kind: "call", func: 1, args: [], keywords: [] },
      ],
    });
    expect(() => evalAst(astNow, ctxNoClock)).toThrow(
      "ValueError: datetime.now() needs a clock: the host must provide one with createContext({ clock })",
    );

    // With clock, datetime.now() returns fixed instant
    const ctxClock = createContext({}, undefined, fixedClock);
    const resNow = evalAst(astNow, ctxClock);
    expect(repr(resNow)).toBe("datetime.datetime(2026, 10, 15, 12, 34, 56)");

    // time.time() returns unix timestamp
    const astTime = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "time" },
        { kind: "attribute", value: 0, attr: "time" },
        { kind: "call", func: 1, args: [], keywords: [] },
      ],
    });
    const resTime = evalAst(astTime, ctxClock);
    expect(resTime).toEqual(floatVal(1792067696.0));

    // time.strftime('%Y-%m')
    const astStrftime = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "time" },
        { kind: "attribute", value: 0, attr: "strftime" },
        { kind: "literal", value: { kind: "str", value: "%Y-%m" } },
        { kind: "call", func: 1, args: [2], keywords: [] },
      ],
    });
    expect(evalAst(astStrftime, ctxClock)).toEqual(strVal("2026-10"));

    // datetime.now('UTC') throws timezone-aware error
    const astTz = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "datetime" },
        { kind: "attribute", value: 0, attr: "now" },
        { kind: "literal", value: { kind: "str", value: "UTC" } },
        { kind: "call", func: 1, args: [2], keywords: [] },
      ],
    });
    expect(() => evalAst(astTz, ctxClock)).toThrow(
      "ValueError: timezone-aware datetimes are not supported in expressions (naive UTC only)",
    );
  });

  it("evaluates relativedelta operations, month end clamping and weekday jumping", () => {
    const ctx = createContext();

    // datetime(2024, 1, 31) + relativedelta(months=1) => datetime(2024, 2, 29, 0, 0)
    const astRd1 = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "datetime" },
        { kind: "literal", value: { kind: "int", value: "2024" } },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "literal", value: { kind: "int", value: "31" } },
        { kind: "call", func: 0, args: [1, 2, 3], keywords: [] },
        { kind: "name", name: "relativedelta" },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "call", func: 5, args: [], keywords: [{ name: "months", value: 6 }] },
        { kind: "binary", op: "add", left: 4, right: 7 },
      ],
    });
    expect(repr(evalAst(astRd1, ctx))).toBe("datetime.datetime(2024, 2, 29, 0, 0)");

    // relativedelta(months=14) => relativedelta(years=+1, months=+2)
    const astRdNormalize = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "relativedelta" },
        { kind: "literal", value: { kind: "int", value: "14" } },
        { kind: "call", func: 0, args: [], keywords: [{ name: "months", value: 1 }] },
      ],
    });
    expect(repr(evalAst(astRdNormalize, ctx))).toBe("relativedelta(years=+1, months=+2)");
  });
});

// -------------------------------------------------------------------------------------------------
// Pre-merge delta fixes (M1-M8, SHOULD items and NITs).
// Every expectation is the kind and message `crates/erp-expr` produces for the same expression.
// -------------------------------------------------------------------------------------------------

type NodeId = number;

/** Builds a wire AST bottom-up; the last node added is the root. */
class AstBuilder {
  readonly nodes: unknown[] = [];

  private add(node: unknown): NodeId {
    this.nodes.push(node);
    return this.nodes.length - 1;
  }

  name(name: string): NodeId {
    return this.add({ kind: "name", name });
  }

  int(value: bigint | number): NodeId {
    return this.add({ kind: "literal", value: { kind: "int", value: String(value) } });
  }

  float(value: number): NodeId {
    const view = new DataView(new ArrayBuffer(8));
    view.setFloat64(0, value);
    const bits = view.getBigUint64(0).toString(16).padStart(16, "0");
    return this.add({ kind: "literal", value: { kind: "float", value: bits } });
  }

  str(value: string): NodeId {
    return this.add({ kind: "literal", value: { kind: "str", value } });
  }

  none(): NodeId {
    return this.add({ kind: "literal", value: { kind: "none" } });
  }

  attr(value: NodeId, attr: string): NodeId {
    return this.add({ kind: "attribute", value, attr });
  }

  call(func: NodeId, args: NodeId[] = [], keywords: [string, NodeId][] = []): NodeId {
    return this.add({
      kind: "call",
      func,
      args,
      keywords: keywords.map(([name, value]) => ({ name, value })),
    });
  }

  binary(op: string, left: NodeId, right: NodeId): NodeId {
    return this.add({ kind: "binary", op, left, right });
  }

  neg(operand: NodeId): NodeId {
    return this.add({ kind: "unary", op: "u_sub", operand });
  }

  /** `datetime(y, m, d, ...)`. */
  datetime(...fields: number[]): NodeId {
    const f = this.name("datetime");
    const args = fields.map((v) => this.int(v));
    return this.call(f, args);
  }

  /** `datetime(y, m, d).date()`. */
  date(y: number, m: number, d: number): NodeId {
    const dt = this.datetime(y, m, d);
    return this.call(this.attr(dt, "date"));
  }

  /** `<recv>.<method>(args..., **kwargs)`. */
  method(recv: NodeId, method: string, args: NodeId[] = [], keywords: [string, NodeId][] = []): NodeId {
    return this.call(this.attr(recv, method), args, keywords);
  }

  /** `relativedelta(**kwargs)` with each value built by the callback. */
  rd(keywords: [string, (b: AstBuilder) => NodeId][]): NodeId {
    const f = this.name("relativedelta");
    const kws: [string, NodeId][] = keywords.map(([k, build]) => [k, build(this)]);
    return this.call(f, [], kws);
  }
}

function run(build: (b: AstBuilder) => void, ctx = createContext()): ExprValue {
  const b = new AstBuilder();
  build(b);
  return evalAst(decode({ schema: 1, nodes: b.nodes }), ctx);
}

function expectPy(fn: () => unknown, className: string, detail: string): void {
  let caught: unknown = undefined;
  try {
    fn();
  } catch (e) {
    caught = e;
  }
  expect(caught).toBeInstanceOf(PythonError);
  const err = caught as PythonError;
  expect(`${err.className}: ${err.messageDetail}`).toBe(`${className}: ${detail}`);
}

function expectRunError(build: (b: AstBuilder) => void, className: string, detail: string, ctx = createContext()): void {
  expectPy(() => run(build, ctx), className, detail);
}

const I64_MAX = 9223372036854775807n;
const I64_MIN = -9223372036854775808n;

describe("DATES delta: method binding (M1, methods.rs date_method / Sig::bind)", () => {
  it("rejects surplus positional, unknown keyword and duplicate arguments of replace", () => {
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "replace", [b.int(2025)], [["year", b.int(2026)]]),
      "TypeError",
      "replace() got multiple values for argument 'year'",
    );
    expectRunError(
      (b) => b.method(b.date(2024, 1, 31), "replace", [], [["unknown", b.int(1)]]),
      "TypeError",
      "replace() got an unexpected keyword argument 'unknown'",
    );
    expectRunError(
      (b) => b.method(b.date(2024, 1, 31), "replace", [b.int(1), b.int(2), b.int(3), b.int(4)]),
      "TypeError",
      "replace() takes at most 3 arguments (4 given)",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "replace", [1, 2, 3, 4, 5, 6, 7, 8].map((v) => b.int(v))),
      "TypeError",
      "replace() takes at most 7 arguments (8 given)",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "replace", [], [["hour", b.str("x")]]),
      "TypeError",
      "replace() argument 'hour' must be int, not str",
    );
  });

  it("validates the date of replace before converting the time fields", () => {
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "replace", [], [["month", b.int(13)], ["hour", b.str("x")]]),
      "ValueError",
      "month must be in 1..12",
    );
  });

  it("rejects arguments to weekday, isoweekday, date and total_seconds", () => {
    expectRunError(
      (b) => b.method(b.date(2024, 1, 31), "weekday", [b.int(1)]),
      "TypeError",
      "weekday() takes no positional arguments (1 given)",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "isoweekday", [], [["x", b.int(1)]]),
      "TypeError",
      "isoweekday() got an unexpected keyword argument 'x'",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "date", [b.int(1)]),
      "TypeError",
      "date() takes no positional arguments (1 given)",
    );
    expectRunError(
      (b) => b.method(b.call(b.name("timedelta"), [b.int(1)]), "total_seconds", [b.int(1)]),
      "TypeError",
      "total_seconds() takes no positional arguments (1 given)",
    );
  });

  it("binds isoformat like Rust for date and datetime", () => {
    expectRunError(
      (b) => b.method(b.date(2024, 1, 31), "isoformat", [b.str(" ")]),
      "TypeError",
      "isoformat() takes no positional arguments (1 given)",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "isoformat", [b.str(" "), b.str("x")]),
      "TypeError",
      "isoformat() takes at most 1 argument (2 given)",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "isoformat", [], [["sep", b.int(1)]]),
      "TypeError",
      "isoformat() argument 'sep' must be str, not int",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "isoformat", [], [["sep", b.str("ab")]]),
      "TypeError",
      "isoformat() argument 'sep' must be a single character",
    );
    // One Unicode scalar outside the BMP is a single character (Rust counts chars, not UTF-16 units).
    expect(run((b) => b.method(b.datetime(2024, 1, 31), "isoformat", [b.str("\u{1F600}")]))).toEqual(
      strVal("2024-01-31\u{1F600}00:00:00"),
    );
  });

  it("binds strftime with a keyword-capable format parameter", () => {
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "strftime"),
      "TypeError",
      "strftime() missing 1 required positional argument: 'format'",
    );
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "strftime", [b.str("%Y"), b.str("%m")]),
      "TypeError",
      "strftime() takes at most 1 argument (2 given)",
    );
    expectRunError(
      (b) => b.method(b.date(2024, 1, 31), "strftime", [], [["fmt", b.str("%Y")]]),
      "TypeError",
      "strftime() got an unexpected keyword argument 'fmt'",
    );
    expect(run((b) => b.method(b.date(2024, 1, 31), "strftime", [], [["format", b.str("%Y")]]))).toEqual(
      strVal("2024"),
    );
  });

  it("does not expose relativedelta.weekday as an attribute, as Rust does not (F3)", () => {
    expectRunError(
      (b) => b.attr(b.rd([["weekday", (x) => x.int(0)]]), "weekday"),
      "AttributeError",
      "'relativedelta' object has no attribute 'weekday'",
    );
  });
});

describe("DATES delta: clock functions (M2, builtins/dates.rs)", () => {
  const clockCtx = () => createContext({}, undefined, fixedClock);

  it("binds datetime.now and datetime.today", () => {
    expectRunError(
      (b) => b.call(b.attr(b.name("datetime"), "now"), [], [["zone", b.none()]]),
      "TypeError",
      "now() got an unexpected keyword argument 'zone'",
      clockCtx(),
    );
    expectRunError(
      (b) => b.call(b.attr(b.name("datetime"), "now"), [b.none()], [["tz", b.none()]]),
      "TypeError",
      "now() got multiple values for argument 'tz'",
      clockCtx(),
    );
    expectRunError(
      (b) => b.call(b.attr(b.name("datetime"), "today"), [], [["x", b.int(1)]]),
      "TypeError",
      "today() got an unexpected keyword argument 'x'",
      clockCtx(),
    );
    expect(repr(run((b) => b.call(b.attr(b.name("datetime"), "now"), [], [["tz", b.none()]]), clockCtx()))).toBe(
      "datetime.datetime(2026, 10, 15, 12, 34, 56)",
    );
  });

  it("binds time.time and time.strftime (format is positional-only)", () => {
    expectRunError(
      (b) => b.call(b.attr(b.name("time"), "time"), [], [["x", b.int(1)]]),
      "TypeError",
      "time() got an unexpected keyword argument 'x'",
      clockCtx(),
    );
    expectRunError(
      (b) => b.call(b.attr(b.name("time"), "strftime"), [], [["format", b.str("%Y")]]),
      "TypeError",
      "strftime() got an unexpected keyword argument 'format'",
      clockCtx(),
    );
    expectRunError(
      (b) => b.call(b.attr(b.name("time"), "strftime"), [b.int(1)]),
      "TypeError",
      "strftime() argument 'format' must be str, not int",
      clockCtx(),
    );
    expectRunError(
      (b) => b.call(b.attr(b.name("time"), "strftime"), [b.str("%Y"), b.str("%m")]),
      "TypeError",
      "strftime() takes at most 1 argument (2 given)",
      clockCtx(),
    );
  });
});

describe("DATES delta: attribute errors of classes and modules (M3, error.rs)", () => {
  it("uses the type-object form for datetime and the module form for time", () => {
    expectRunError(
      (b) => b.attr(b.name("datetime"), "utcnow"),
      "AttributeError",
      "type object 'datetime.datetime' has no attribute 'utcnow'",
    );
    expectRunError((b) => b.attr(b.name("time"), "sleep"), "AttributeError", "module 'time' has no attribute 'sleep'");
  });
});

describe("DATES delta: strftime (M4, NIT surrogate, SHOULD budget)", () => {
  it("prints %Y without zero padding while %F keeps the ISO form", () => {
    expect(run((b) => b.method(b.datetime(5, 1, 1), "strftime", [b.str("%Y|%F")]))).toEqual(strVal("5|0005-01-01"));
  });

  it("names a directive outside the BMP whole in the error", () => {
    expectRunError(
      (b) => b.method(b.datetime(2024, 1, 31), "strftime", [b.str("%\u{1F600}")]),
      "ValueError",
      "unsupported strftime directive '%\u{1F600}'",
    );
  });

  it("charges the format scan and the output like Budget::scan/out/finish", () => {
    const budget = new Budget();
    const out = strftime({ year: 2024, month: 1, day: 31 }, { hour: 0, minute: 0, second: 0, microsecond: 0 }, "%Y-%m-%d", budget);
    expect(out).toBe("2024-01-31");
    // scan(8 format bytes) = 1 step, finish: scan(10 output bytes) = 1 step and alloc(10).
    expect(budget.stepsUsed).toBe(2);
    expect(budget.bytesUsed).toBe(10);
  });

  it("caps the output at the per-value limit and at the remaining total budget", () => {
    expectPy(
      () =>
        strftime({ year: 2024, month: 1, day: 31 }, { hour: 0, minute: 0, second: 0, microsecond: 0 }, "%Y-%m-%d", new Budget({ maxValueBytes: 4 })),
      "LimitExceeded",
      "string exceeds the per-value limit of 4 bytes",
    );
    const budget = new Budget({ maxTotalBytes: 100 });
    budget.charge(95);
    expectPy(
      () => strftime({ year: 2024, month: 1, day: 31 }, { hour: 0, minute: 0, second: 0, microsecond: 0 }, "%Y-%m-%d", budget),
      "LimitExceeded",
      "evaluation memory budget of 100 bytes exceeded",
    );
  });
});

describe("DATES delta: datetime() validation order and exact ranges (M5, M6)", () => {
  it("validates the date before converting the time fields", () => {
    expectRunError(
      (b) => b.call(b.name("datetime"), [b.int(2024), b.int(13), b.int(1), b.str("x")]),
      "ValueError",
      "month must be in 1..12",
    );
    expectRunError(
      (b) => b.call(b.name("datetime"), [b.int(2024), b.int(1), b.int(1), b.int(24), b.str("x")]),
      "TypeError",
      "datetime() argument 'minute' must be int, not str",
    );
  });

  it("prints i64 years exactly in range errors", () => {
    expectRunError(
      (b) => b.call(b.name("datetime"), [b.int(I64_MAX), b.int(1), b.int(1)]),
      "ValueError",
      "year 9223372036854775807 is out of range",
    );
    expectRunError(
      (b) => b.method(b.date(2024, 1, 1), "replace", [], [["year", b.int(I64_MIN)]]),
      "ValueError",
      "year -9223372036854775808 is out of range",
    );
    expectRunError(
      (b) => b.binary("add", b.datetime(2024, 1, 1), b.rd([["year", (x) => x.int(I64_MAX)]])),
      "ValueError",
      "year 9223372036854775807 is out of range",
    );
  });
});

describe("DATES delta: relativedelta floats (M7, num.rs float_as_int, args.rs int_arg)", () => {
  it("rejects integral floats outside i64 like Rust", () => {
    expectRunError((b) => b.rd([["days", (x) => x.float(1e19)]]), "OverflowError", "date value out of range");
    expectRunError(
      (b) => b.rd([["years", (x) => x.float(1e19)]]),
      "ValueError",
      "Non-integer years and months are ambiguous and not currently supported.",
    );
    expectRunError(
      (b) => b.rd([["year", (x) => x.float(1e19)]]),
      "ValueError",
      "relativedelta argument 'year' must be an integer, got 1e+19",
    );
  });

  it("prints non-integer floats with Python repr", () => {
    expectRunError(
      (b) => b.rd([["year", (x) => x.float(Number.NaN)]]),
      "ValueError",
      "relativedelta argument 'year' must be an integer, got nan",
    );
    expectRunError(
      (b) => b.rd([["hour", (x) => x.float(Number.POSITIVE_INFINITY)]]),
      "ValueError",
      "relativedelta argument 'hour' must be an integer, got inf",
    );
    expectRunError(
      (b) => b.rd([["leapdays", (x) => x.float(1.5)]]),
      "ValueError",
      "relativedelta argument 'leapdays' must be an integer, got 1.5",
    );
    expectRunError((b) => b.rd([["days", (x) => x.float(Number.NaN)]]), "OverflowError", "timedelta value out of range");
  });

  it("names relativedelta() in the int type error", () => {
    expectRunError(
      (b) => b.rd([["year", (x) => x.str("x")]]),
      "TypeError",
      "relativedelta() argument 'year' must be int, not str",
    );
  });

  it("accepts integral floats inside i64", () => {
    expect(repr(run((b) => b.rd([["days", (x) => x.float(2)]])))).toBe("relativedelta(days=+2)");
  });
});

describe("DATES delta: relativedelta checked i64 arithmetic (M8, datetime.rs)", () => {
  it("keeps i64 field values exact", () => {
    expect(repr(run((b) => b.rd([["days", (x) => x.int(9007199254740993n)]])))).toBe(
      "relativedelta(days=+9007199254740993)",
    );
    expect(run((b) => b.attr(b.rd([["days", (x) => x.int(9007199254740993n)]]), "days"))).toEqual(
      intVal(9007199254740993n),
    );
  });

  it("raises OverflowError on overflowing construction", () => {
    expectRunError((b) => b.rd([["weeks", (x) => x.int(I64_MAX)]]), "OverflowError", "date value out of range");
    expectRunError(
      (b) => b.rd([["weeks", (x) => x.int(1)], ["days", (x) => x.int(I64_MAX)]]),
      "OverflowError",
      "date value out of range",
    );
    expectRunError(
      (b) => b.rd([["microseconds", (x) => x.int(I64_MIN)]]),
      "OverflowError",
      "date value out of range",
    );
    expectRunError(
      (b) => b.rd([["hours", (x) => x.int(I64_MAX)], ["days", (x) => x.int(I64_MAX)]]),
      "OverflowError",
      "date value out of range",
    );
  });

  it("raises OverflowError on overflowing plus, negation and calendar arithmetic", () => {
    expectRunError(
      (b) => b.binary("add", b.rd([["days", (x) => x.int(I64_MAX)]]), b.rd([["days", (x) => x.int(1)]])),
      "OverflowError",
      "date value out of range",
    );
    expectRunError((b) => b.neg(b.rd([["days", (x) => x.int(I64_MIN)]])), "OverflowError", "date value out of range");
    expectRunError(
      (b) => b.binary("add", b.datetime(2024, 1, 1), b.rd([["years", (x) => x.int(I64_MAX)]])),
      "OverflowError",
      "date value out of range",
    );
    expectRunError(
      (b) => b.binary("add", b.datetime(2024, 3, 1), b.rd([["days", (x) => x.int(I64_MAX)], ["leapdays", (x) => x.int(1)]])),
      "OverflowError",
      "date value out of range",
    );
    expectRunError(
      (b) => b.binary("add", b.datetime(2024, 1, 1), b.rd([["days", (x) => x.int(1_000_000_000)]])),
      "OverflowError",
      "days=1000000000; must have magnitude <= 999999999",
    );
  });
});

describe("DATES delta: timedelta (SHOULD items, ops.rs mul, builtins/dates.rs timedelta)", () => {
  it("reports i128 overflow of timedelta * int as a date range error", () => {
    expectRunError(
      (b) => b.binary("mult", b.call(b.name("timedelta"), [b.int(999_999_999)]), b.int(I64_MAX)),
      "OverflowError",
      "date value out of range",
    );
    expectRunError(
      (b) => b.binary("mult", b.call(b.name("timedelta"), [b.int(1)]), b.int(I64_MAX)),
      "OverflowError",
      "days=9223372036854775807; must have magnitude <= 999999999",
    );
  });

  it("binds arguments before converting them and converts in parameter order", () => {
    expectRunError(
      (b) => b.call(b.name("timedelta"), [b.str("x")], [["foo", b.int(1)]]),
      "TypeError",
      "timedelta() got an unexpected keyword argument 'foo'",
    );
    expectRunError(
      (b) => b.call(b.name("timedelta"), [], [["hours", b.str("x")], ["seconds", b.str("y")]]),
      "TypeError",
      "unsupported type for timedelta seconds component: str",
    );
  });
});

describe("DATES delta: Command budget (SHOULD, builtins/command.rs)", () => {
  function callCommand(method: string, args: ExprValue[], budget: Budget): ExprValue {
    const fn = new CommandNamespace().getAttr(method, budget);
    if (fn.kind !== "callable") throw new Error("Command method is not callable");
    return fn.call(args, {}, budget);
  }

  it("admits the values dict of create and update and the ids of set", () => {
    const values = dictVal([[strVal("name"), strVal("x")]]);
    const ids = listVal([intVal(1n), intVal(2n)]);
    for (const [method, args, admitted] of [
      ["create", [values], values],
      ["update", [intVal(3n), values], values],
      ["set", [ids], ids],
    ] as const) {
      const budget = new Budget();
      callCommand(method, [...args], budget);
      expect(budget.bytesUsed).toBe(computeDeepBytes(admitted) + 4 * SLOT);
      expect(budget.stepsUsed).toBe(computeNodes(admitted));
    }
  });

  it("checks the depth of the built triple", () => {
    const values = dictVal([[strVal("a"), listVal([intVal(1n)])]]);
    const budget = new Budget({ maxDepth: computeDepth(values) });
    expectPy(
      () => callCommand("create", [values], budget),
      "LimitExceeded",
      `value nesting depth limit of ${computeDepth(values)} exceeded`,
    );
  });
});

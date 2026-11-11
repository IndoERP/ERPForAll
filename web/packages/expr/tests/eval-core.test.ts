/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, it } from "vitest";
import {
  boolVal,
  createContext,
  decodeAst,
  evalAst,
  intVal,
  keysEqual,
  objectVal,
  parseBoundedJson,
  PyDict,
  PythonError,
  repr,
  strVal,
  type ExprObject,
  type ExprValue,
  type ValidatedAst,
} from "../src/index";

function decode(obj: unknown): ValidatedAst {
  return decodeAst(parseBoundedJson(JSON.stringify(obj)));
}

function floatVal(v: number): ExprValue {
  return { kind: "float", value: v };
}

describe("CORE client evaluator", () => {
  it("enforces Python dict key equality and prototype safety", () => {
    // 0 == 0.0 == False
    expect(keysEqual(intVal(0n), floatVal(0.0))).toBe(true);
    expect(keysEqual(intVal(0n), boolVal(false))).toBe(true);
    expect(keysEqual(floatVal(0.0), boolVal(false))).toBe(true);

    // 1 == 1.0 == True
    expect(keysEqual(intVal(1n), floatVal(1.0))).toBe(true);
    expect(keysEqual(intVal(1n), boolVal(true))).toBe(true);
    expect(keysEqual(floatVal(1.0), boolVal(true))).toBe(true);

    // Unhashable types throw TypeError
    const listKey = { kind: "list" as const, items: [] };
    expect(() => keysEqual(listKey, intVal(1n))).toThrow(/TypeError: unhashable type: 'list'/);

    // Prototype safety
    const dict = new PyDict();
    dict.set(strVal("__proto__"), strVal("hacked"));
    dict.set(strVal("constructor"), strVal("polluted"));

    // Ensure Object.prototype is unmodified
    expect((Object.prototype as Record<string, unknown>)["__proto__"]).not.toBe("hacked");
    expect({}.constructor).toBe(Object);
    expect(dict.get(strVal("__proto__"))).toEqual(strVal("hacked"));
  });

  it("handles BigInt i64 checked arithmetic and overflow", () => {
    // AST for: 9223372036854775807 + 1
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "9223372036854775807" } },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "binary", op: "add", left: 0, right: 1 },
      ],
    });
    const ctx = createContext();
    let failure: unknown;
    try { evalAst(ast, ctx); }
    catch (error) { failure = error; }
    expect(failure).toBeInstanceOf(PythonError);
    expect(failure).toMatchObject({
      className: "OverflowError",
      message: "OverflowError: integer result does not fit in 64 bits",
    });
  });

  it("evaluates negative floor division and modulo per Python semantics", () => {
    // -7 // 2 = -4
    const ast1 = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "7" } },
        { kind: "unary", op: "u_sub", operand: 0 },
        { kind: "literal", value: { kind: "int", value: "2" } },
        { kind: "binary", op: "floor_div", left: 1, right: 2 },
      ],
    });
    const ctx = createContext();
    expect(evalAst(ast1, ctx)).toEqual(intVal(-4n));

    // -7 % 3 = 2
    const ast2 = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "7" } },
        { kind: "unary", op: "u_sub", operand: 0 },
        { kind: "literal", value: { kind: "int", value: "3" } },
        { kind: "binary", op: "mod", left: 1, right: 2 },
      ],
    });
    expect(evalAst(ast2, ctx)).toEqual(intVal(2n));
  });

  it("evaluates chained comparisons with single evaluation and short-circuit", () => {
    // 1 < 3 < 2 => False
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "literal", value: { kind: "int", value: "3" } },
        { kind: "literal", value: { kind: "int", value: "2" } },
        { kind: "compare", left: 0, ops: ["lt", "lt"], comparators: [1, 2] },
      ],
    });
    const ctx = createContext();
    expect(evalAst(ast, ctx)).toEqual(boolVal(false));
  });

  it("handles Unicode code-point indexing and slicing", () => {
    // 'héllo wörld'[::-1] => 'dlröw olléh'
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "str", value: "héllo wörld" } },
        { kind: "literal", value: { kind: "int", value: "-1" } },
        { kind: "subscript", value: 0, slice: { kind: "slice", lower: null, upper: null, step: 1 } },
      ],
    });
    const ctx = createContext();
    expect(evalAst(ast, ctx)).toEqual(strVal("dlröw olléh"));
  });

  it("supports walrus bindings and comprehensions", () => {
    // (y := 5) + y => 10
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "5" } },
        { kind: "walrus", name: "y", value: 0 },
        { kind: "name", name: "y" },
        { kind: "binary", op: "add", left: 1, right: 2 },
      ],
    });
    const ctx = createContext();
    expect(evalAst(ast, ctx)).toEqual(intVal(10n));
  });

  it("resolves lazy Object attributes only when accessed", () => {
    let accessCount = 0;
    const testObj: ExprObject = {
      typeName: () => "TestRecord",
      getAttr: (name: string) => {
        accessCount++;
        if (name === "active") return boolVal(false);
        if (name === "expensive") return intVal(42n);
        throw new Error(`AttributeError: 'TestRecord' object has no attribute '${name}'`);
      },
    };

    const ctx = createContext({
      record: objectVal(testObj),
    });

    // record.expensive if record.active else 0
    // Because active is false, record.expensive must NOT be evaluated!
    const ast = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "record" },
        { kind: "attribute", value: 0, attr: "active" },
        { kind: "name", name: "record" },
        { kind: "attribute", value: 2, attr: "expensive" },
        { kind: "literal", value: { kind: "int", value: "0" } },
        { kind: "if_exp", test: 1, body: 3, orelse: 4 },
      ],
    });

    const res = evalAst(ast, ctx);
    expect(res).toEqual(intVal(0n));
    expect(accessCount).toBe(1); // Only active was accessed, expensive was never read!
  });

  it("evaluates integrated FORMAT and DATES operations", () => {
    const ctx = createContext();

    // formatted_value node -> evaluates in FORMAT
    const astFormat = decode({
      schema: 1,
      nodes: [
        { kind: "literal", value: { kind: "int", value: "42" } },
        { kind: "formatted_value", value: 0, conversion: null, format_spec: null },
      ],
    });
    expect(evalAst(astFormat, ctx)).toEqual(strVal("42"));

    // datetime builtin call ->
    const astDates = decode({
      schema: 1,
      nodes: [
        { kind: "name", name: "datetime" },
        { kind: "literal", value: { kind: "int", value: "2024" } },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "literal", value: { kind: "int", value: "1" } },
        { kind: "call", func: 0, args: [1, 2, 3], keywords: [] },
      ],
    });
    expect(repr(evalAst(astDates, ctx))).toBe("datetime.datetime(2024, 1, 1, 0, 0)");
  });
});

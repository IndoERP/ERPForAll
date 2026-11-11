/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Budget, PythonError, createContext, decodeAst, evalAst, parseBoundedJson,
  intVal, listVal, bytesVal, objectVal, callableVal, CommandNamespace, strVal, dictVal, type ExprValue } from "../src/index";

function pyError(fn: () => unknown, kind: string, message: string): void {
  let error: unknown;
  try { fn(); } catch (e) { error = e; }
  expect(error).toBeInstanceOf(PythonError);
  expect((error as PythonError).className).toBe(kind);
  expect((error as PythonError).messageDetail).toBe(message);
}
function ast(nodes: unknown[]) {
  return decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes })));
}
const nameAst = ast([{ kind: "name", name: "x" }]);

/// (partial: cyclic, sparse and forged host values fail before recursive work)
test("host admission rejects cycles, sparse values and forged payloads", () => {
  const cyclic = { kind: "list", items: [] as unknown[] };
  cyclic.items.push(cyclic);
  pyError(() => new Budget().admit(cyclic as ExprValue), "ValueError", "cyclic expression value");
  for (const value of [{ kind: "int", value: "1" }, { kind: "bool", value: 1 },
    { kind: "list", items: new Array(2) }, { kind: "wat" }]) {
    pyError(() => new Budget().admit(value as ExprValue), "TypeError", "invalid expression value");
  }
});

/// (partial: host depth and oversized flat containers stop before visiting payloads)
test("host admission checks bounds before reading children", () => {
  let value = intVal(1n) as ExprValue;
  for (let i = 0; i < 1000; i++) value = { kind: "list", items: [value] };
  pyError(() => new Budget({ maxDepth: 5 }).admit(value), "LimitExceeded", "value nesting depth limit of 5 exceeded");
  let reads = 0;
  const items = new Array(100);
  Object.defineProperty(items, 0, { get() { reads++; throw new Error("must not read"); } });
  pyError(() => new Budget({ maxValueBytes: 64 }).admit({ kind: "list", items }), "LimitExceeded", "value of 3232 bytes exceeds the per-value limit of 64 bytes");
  expect(reads).toBe(0);
});

/// (partial: admission refuses getters and unregistered capability tags)
test("data getters and forged capabilities are refused without execution", () => {
  let reads = 0;
  const raw = { kind: "int" as const, get value() { reads++; return 1n; } };
  pyError(() => new Budget().admit(raw), "TypeError", "invalid expression value");
  expect(reads).toBe(0);
  pyError(() => new Budget().admit({ kind: "callable", name: "x", call: () => intVal(1n) }), "TypeError", "invalid expression value");
});

/// (partial: name and resolver reads snapshot data before host mutation)
test("names and resolver results receive immutable snapshots and charge admission", () => {
  const bytes = new Uint8Array([1, 2]);
  const items: ExprValue[] = [bytesVal(bytes)];
  const raw: ExprValue = { kind: "list", items };
  for (const ctx of [createContext({ x: raw }), createContext({}, undefined, undefined, () => raw)]) {
    const result = evalAst(nameAst, ctx);
    bytes[0] = 7;
    items.push(intVal(9n));
    expect(result).toEqual(listVal([bytesVal([1, 2])]));
    items.pop(); bytes[0] = 1;
  }
  pyError(() => evalAst(nameAst, createContext({ x: raw }, { maxTotalBytes: 1 })), "LimitExceeded", "evaluation memory budget of 1 bytes exceeded");
});

/// (partial: host attributes and calls validate their returned data)
test("host results are admitted at attribute and call boundaries", () => {
  const bad = { kind: "int", value: "forged" } as unknown as ExprValue;
  const attr = ast([{ kind: "name", name: "x" }, { kind: "attribute", value: 0, attr: "a" }]);
  const call = ast([{ kind: "name", name: "x" }, { kind: "call", func: 0, args: [], keywords: [] }]);
  pyError(() => evalAst(attr, createContext({ x: objectVal({ typeName: () => "Test", getAttr: () => bad }) })), "TypeError", "invalid expression value");
  pyError(() => evalAst(call, createContext({ x: callableVal("host", () => bad) })), "TypeError", "invalid expression value");
});

/// (partial: host integers obey the evaluator i64 domain)
test("host integers outside i64 are refused", () => {
  pyError(() => new Budget().admit(intVal(10n ** 400n)), "OverflowError", "integer overflow");
});

function callBuiltin(name: string, args: ExprValue[], kwargs: Record<string, ExprValue> = {}) {
  const fn = createContext().names[name]!;
  if (fn.kind !== "callable") throw new Error("test builtin is not callable");
  return fn.call(args, kwargs, new Budget());
}

/// (partial: builtin keyword arguments cannot be silently ignored)
test("builtins refuse unknown and duplicated keywords", () => {
  for (const name of ["round", "min", "max", "sum"]) {
    pyError(() => callBuiltin(name, [intVal(1n)], { bogus: intVal(1n) }), "TypeError", `${name}() got an unexpected keyword argument 'bogus'`);
  }
  for (const name of ["list", "tuple"]) {
    pyError(() => callBuiltin(name, [], { bogus: intVal(1n) }), "TypeError", `${name}() got an unexpected keyword argument 'bogus'`);
  }
  pyError(() => callBuiltin("round", [intVal(1n), intVal(1n)], { ndigits: intVal(1n) }), "TypeError", "round() got multiple values for argument 'ndigits'");
  pyError(() => callBuiltin("sum", [listVal([]), intVal(1n)], { start: intVal(1n) }), "TypeError", "sum() got multiple values for argument 'start'");
});

/// (partial: round precision cannot request unbounded powers)
test("round handles extreme precision without a native exception or NaN", () => {
  const float = (value: number) => ({ kind: "float", value } as const);
  expect(callBuiltin("round", [float(1.25), intVal(9223372036854775807n)])).toEqual(float(1.25));
  expect(callBuiltin("round", [float(-1.25), intVal(-9223372036854775808n)])).toEqual(float(-0));
  pyError(() => callBuiltin("round", [float(Infinity)]), "OverflowError", "cannot convert float infinity to integer");
  pyError(() => callBuiltin("round", [float(NaN)]), "ValueError", "cannot convert float NaN to integer");
});

/// (partial: __proto__ bindings are ordinary evaluation-local variables)
test("walrus bindings cannot mutate the scope prototype", () => {
  const value = ast([{ kind: "literal", value: { kind: "int", value: "7" } },
    { kind: "walrus", name: "__proto__", value: 0 },
    { kind: "name", name: "__proto__" },
    { kind: "tuple", items: [1, 2] }]);
  expect(evalAst(value, createContext())).toEqual({ kind: "tuple", items: [intVal(7n), intVal(7n)] });
});

/// (partial: numeric comparison keeps exact i64 values above 2^53)
test("numeric comparisons preserve adjacent i64 and mixed float ordering", () => {
  const a = 9007199254740992n, b = a + 1n;
  expect(callBuiltin("min", [intVal(b), intVal(a)])).toEqual(intVal(a));
  expect(callBuiltin("max", [intVal(a), intVal(b)])).toEqual(intVal(b));
  const compare = (op: string, x: ExprValue, y: ExprValue) => evalAst(ast([
    { kind: "name", name: "x" }, { kind: "name", name: "y" },
    { kind: "compare", left: 0, ops: [op], comparators: [1] },
  ]), createContext({ x, y }));
  expect(compare("lt", intVal(a), intVal(b))).toEqual({ kind: "bool", value: true });
  expect(compare("eq", intVal(b), { kind: "float", value: Number(a) })).toEqual({ kind: "bool", value: false });
  expect(compare("gt", intVal(b), { kind: "float", value: Number(a) })).toEqual({ kind: "bool", value: true });
});

/// (partial: invalid host limits cannot disable budget checks)
test("invalid budget limits fail instead of disabling admission", () => {
  for (const value of [NaN, Infinity, -1, 0, 1.5]) {
    pyError(() => new Budget({ maxSteps: value }), "ValueError", "evaluation limit 'maxSteps' must be a positive safe integer");
  }
});

/// (partial: sum list concatenation retains budget charging)
test("sum cannot bypass list concatenation budget", () => {
  const sum = createContext().names["sum"]!;
  if (sum.kind !== "callable") throw new Error("missing sum builtin");
  pyError(() => sum.call([listVal([listVal([intVal(1n), intVal(2n)])]), listVal([])], {},
    new Budget({ maxTotalBytes: 32 })), "LimitExceeded", "evaluation memory budget of 32 bytes exceeded");
});

/// (partial: Command retains the admitted snapshot rather than raw host references)
test("Command builders retain snapshots of admitted arguments", () => {
  const values: ExprValue[] = [intVal(1n)];
  const raw = { kind: "list", items: values } as const;
  for (const [method, argument] of [["set", raw], ["create", dictVal([[strVal("x"), raw]])]] as const) {
    const budget = new Budget();
    const fn = new CommandNamespace().getAttr(method, budget);
    if (fn.kind !== "callable") throw new Error("missing Command method");
    const result = fn.call([argument], {}, budget);
    values.push(intVal(2n));
    const expected = method === "set" ? listVal([intVal(1n)]) : dictVal([[strVal("x"), listVal([intVal(1n)])]]);
    expect(result).toEqual({ kind: "tuple", items: [intVal(method === "set" ? 6n : 0n), intVal(0n), expected] });
    values.pop();
  }
});

/// (partial: trusted sequence constructors bound input before copying or using an iterator)
test("sequence constructors refuse oversized or malformed input before copying", () => {
  let reads = 0;
  const items = new Array<ExprValue>(100_000);
  Object.defineProperty(items, 0, { get() { reads++; return intVal(1n); } });
  pyError(() => listVal(items), "LimitExceeded", "value of 3200032 bytes exceeds the per-value limit of 1048576 bytes");
  expect(reads).toBe(0);
  pyError(() => listVal(new Array(1)), "TypeError", "invalid expression value");
  const valid = [intVal(1n)];
  Object.defineProperty(valid, Symbol.iterator, { value() { reads++; throw new Error("data iterator executed"); } });
  expect(listVal(valid)).toEqual({ kind: "list", items: [intVal(1n)] });
  expect(reads).toBe(0);
});

/// (partial: malformed nested shapes and self/two-node cycles are refused)
test("nested host shape validation rejects forged dates, pairs and cycles", () => {
  const a = { kind: "list", items: [] as unknown[] };
  const b = { kind: "tuple", items: [a] };
  a.items.push(b);
  pyError(() => new Budget().admit(a as ExprValue), "ValueError", "cyclic expression value");
  for (const raw of [
    { kind: "date", year: 2026, month: 2, day: 30 },
    { kind: "datetime", year: 2026, month: 1, day: 1, hour: 24, minute: 0, second: 0, microsecond: 0 },
    { kind: "dict", entries: [[intVal(1n)]] },
    { kind: "dict", entries: [new Array(2)] },
    { kind: "bytes", value: [1, 2] },
  ]) pyError(() => new Budget().admit(raw as ExprValue), "TypeError", "invalid expression value");
});

/// (partial: shared acyclic values count per occurrence and quota failure stays sticky)
test("admission preserves logical shared-value accounting and sticky errors", () => {
  const child = listVal([strVal("é")]);
  const raw = { kind: "list", items: [child, child] } as const;
  const budget = new Budget({ maxTotalBytes: 164 });
  const result = budget.admit(raw);
  expect(result).toEqual(raw);
  expect(budget.bytesUsed).toBe(164);
  expect(budget.stepsUsed).toBe(5);
  pyError(() => budget.admit(intVal(0n)), "LimitExceeded", "evaluation memory budget of 164 bytes exceeded");
  const first = budget.stickyError;
  let second: unknown;
  try { budget.tick(); } catch (error) { second = error; }
  expect(second).toBe(first);
});

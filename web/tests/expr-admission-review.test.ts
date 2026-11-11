/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Budget, PythonError, createContext, decodeAst, evalAst, parseBoundedJson,
  callableVal, computeDeepBytes, dictVal, intVal, listVal, strVal, NONE,
  type EvalLimits, type ExprValue, type ValidatedAst } from "@erpforall/expr";
import { createRecordExpression, type SessionBindings } from "../src/runtime/expr.js";
import { UIError } from "../src/runtime/errors.js";

function ast(nodes: unknown[]): ValidatedAst {
  return decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes })));
}
function caught(fn: () => unknown): unknown {
  try { fn(); } catch (error) { return error; }
  throw new Error("expected an error");
}
function pyError(fn: () => unknown, kind: string, message: string): void {
  const error = caught(fn);
  expect(error).toBeInstanceOf(PythonError);
  expect((error as PythonError).className).toBe(kind);
  expect((error as PythonError).messageDetail).toBe(message);
}
function uiError(fn: () => unknown, message: string): void {
  const error = caught(fn);
  expect(error).toBeInstanceOf(UIError);
  expect((error as UIError).code).toBe("value");
  expect((error as UIError).message).toBe(message);
}
function session(ids: SessionBindings["allowed_company_ids"] = []): SessionBindings {
  return { uid: 1n, allowed_company_ids: ids, context: {} };
}
function run(input: ValidatedAst, limits: Partial<EvalLimits>, names: Record<string, ExprValue> = {}, adapter = false): ExprValue {
  if (!adapter) return evalAst(input, createContext(names, limits));
  const handle = createRecordExpression(input, { id: false, record: names, session: session(), limits });
  try { return handle.read; } finally { handle.dispose(); }
}
function call(name: string, arg?: string): ValidatedAst {
  return ast(arg === undefined ? [{ kind: "name", name }, { kind: "call", func: 0, args: [], keywords: [] }]
    : [{ kind: "name", name }, { kind: "literal", value: { kind: "str", value: arg } },
      { kind: "call", func: 0, args: [1], keywords: [] }]);
}

/// (partial: effective limits come from the validated own descriptor set)
test("R1 inherited settings cannot disable a limit", () => {
  pyError(() => new Budget(Object.create({ maxSteps: NaN }) as Partial<EvalLimits>),
    "ValueError", "evaluation limits must be a plain data object");
});
/// (partial: non-enumerable limit values are validated)
test("R1 non-enumerable invalid settings cannot disable a limit", () => {
  const input = Object.defineProperty({}, "maxTotalBytes", { value: NaN });
  pyError(() => new Budget(input), "ValueError", "evaluation limit 'maxTotalBytes' must be a positive safe integer");
});
/// (partial: settings accessors never execute during configuration admission)
test("R1 changing limit getters are refused without execution", () => {
  let reads = 0;
  const input = { get maxSteps() { return ++reads === 1 ? 1 : NaN; } };
  pyError(() => new Budget(input), "ValueError", "evaluation limit 'maxSteps' must be a data property");
  expect(reads).toBe(0);
});

for (const adapter of [false, true]) {
  for (const name of ["list", "tuple", "dict"]) {
    /// (partial: empty container production is charged before construction)
    test(`R2 ${name} empty constructor uses the active budget (adapter=${adapter})`, () => {
      expect(run(call(name), { maxTotalBytes: 32 }, {}, adapter).kind).toBe(name);
      pyError(() => run(call(name), { maxTotalBytes: 31 }, {}, adapter), "LimitExceeded", "evaluation memory budget of 31 bytes exceeded");
    });
  }
  for (const name of ["list", "tuple"]) {
    /// (partial: string iterable expansion is charged before copying)
    test(`R2 ${name} string conversion uses active remaining bytes (adapter=${adapter})`, () => {
      pyError(() => run(call(name, "a".repeat(1000)), { maxTotalBytes: 2000 }, {}, adapter),
        "LimitExceeded", "evaluation memory budget of 2000 bytes exceeded");
    });
  }
  const d = dictVal([[strVal("x"), intVal(1n)]]);
  for (const method of ["keys", "values", "items"]) {
    /// (partial: dictionary method outputs reserve the complete result before copying)
    test(`R2 dict.${method} exact and over-bound output (adapter=${adapter})`, () => {
      const input = ast([{ kind: "name", name: "d" }, { kind: "attribute", value: 0, attr: method },
        { kind: "call", func: 1, args: [], keywords: [] }]);
      const expected = method === "keys" ? listVal([strVal("x")]) : method === "values" ? listVal([intVal(1n)])
        : listVal([{ kind: "tuple", items: [strVal("x"), intVal(1n)] }]);
      const cap = computeDeepBytes(d) + computeDeepBytes(expected);
      expect(run(input, { maxTotalBytes: cap }, { d }, adapter)).toEqual(expected);
      pyError(() => run(input, { maxTotalBytes: cap - 1 }, { d }, adapter), "LimitExceeded", `evaluation memory budget of ${cap - 1} bytes exceeded`);
    });
  }
  /// (partial: dictionary copies retain their producer charge)
  test(`R2 dict copy reserves against the active budget (adapter=${adapter})`, () => {
    const input = ast([{ kind: "name", name: "dict" }, { kind: "name", name: "d" },
      { kind: "call", func: 0, args: [1], keywords: [] }]);
    const cap = 2 * computeDeepBytes(d) + 32;
    expect(run(input, { maxTotalBytes: cap }, { d }, adapter)).toEqual(d);
    pyError(() => run(input, { maxTotalBytes: cap - 1 }, { d }, adapter), "LimitExceeded", `evaluation memory budget of ${cap - 1} bytes exceeded`);
  });
}

const fCall = call("f");
for (const resolver of [false, true]) {
  /// (partial: reintroduced bound methods do not inherit internal-budget exemption)
  test(`R3 an exported method uses lowered second-evaluation limits (resolver=${resolver})`, () => {
    const saved = evalAst(ast([{ kind: "literal", value: { kind: "str", value: "abc" } },
      { kind: "attribute", value: 0, attr: "lower" }]), createContext());
    const limits = { maxSteps: 2, maxTotalBytes: 1 };
    const context = resolver ? createContext({}, limits, undefined, () => saved) : createContext({ f: saved }, limits);
    pyError(() => evalAst(fCall, context), "LimitExceeded", "evaluation memory budget of 1 bytes exceeded");
  });
  for (const method of ["lower", "keys"]) {
    /// (partial: a previous evaluation's sticky failure cannot poison a bound method)
    test(`R3 ${method} uses invocation budget after the old one trips (resolver=${resolver})`, () => {
      let previous: Budget | undefined;
      const capture = callableVal("capture", (_args, _kwargs, budget) => { previous = budget; return NONE; });
      const input = ast([
        method === "lower" ? { kind: "literal", value: { kind: "str", value: "abc" } } : { kind: "name", name: "d" },
        { kind: "attribute", value: 0, attr: method }, { kind: "name", name: "capture" },
        { kind: "call", func: 2, args: [], keywords: [] }, { kind: "tuple", items: [1, 3] },
      ]);
      const output = evalAst(input, createContext({ capture, d: dictVal([[strVal("x"), intVal(1n)]]) }));
      if (output.kind !== "tuple" || output.items[0] === undefined || previous === undefined) throw new Error("missing captured method/budget");
      const saved = output.items[0];
      const old = previous;
      pyError(() => old.charge(old.limits.maxTotalBytes), "LimitExceeded", `evaluation memory budget of ${old.limits.maxTotalBytes} bytes exceeded`);
      const context = resolver ? createContext({}, undefined, undefined, () => saved) : createContext({ f: saved });
      expect(evalAst(fCall, context)).toEqual(method === "lower" ? strVal("abc") : listVal([strVal("x")]));
    });
  }
}

const uid = ast([{ kind: "name", name: "uid" }]);
/// (partial: company normalization bounds collections before reading or copying)
test("R4 oversized company ids are refused before the first member", () => {
  let reads = 0;
  const ids = new Array<bigint>(50_000);
  Object.defineProperty(ids, 0, { get() { reads++; return 1n; } });
  uiError(() => createRecordExpression(uid, { id: false, session: session(ids) }),
    "session.allowed_company_ids holds more than 32767 ids");
  expect(reads).toBe(0);
});
/// (partial: company identifiers are own data properties)
test("R4 indexed company-id getters are refused without execution", () => {
  let reads = 0;
  const ids: bigint[] = [];
  Object.defineProperty(ids, 0, { get() { reads++; return 1n; } });
  uiError(() => createRecordExpression(uid, { id: false, session: session(ids) }),
    "session.allowed_company_ids[0] must be an integer data property");
  expect(reads).toBe(0);
});
/// (partial: company normalization ignores host map/iterator overrides)
test("R4 company-id normalization never invokes a host map override", () => {
  let reads = 0;
  const ids = [1n];
  Object.defineProperty(ids, "map", { value() { reads++; return [intVal(2n)]; } });
  const input = ast([{ kind: "name", name: "allowed_company_ids" }]);
  const handle = createRecordExpression(input, { id: false, session: session(ids) });
  try { expect(handle.read).toEqual(listVal([intVal(1n)])); } finally { handle.dispose(); }
  expect(reads).toBe(0);
});

for (const adapter of [false, true]) for (const name of ["min", "max", "sum"]) {
  /// (partial: scalar reducers retain iterable-copy and selected-result charging)
  test(`R2 ${name} retains its iterable admission events (adapter=${adapter})`, () => {
    const values = listVal([intVal(1n)]);
    const input = ast([{ kind: "name", name }, { kind: "name", name: "values" },
      { kind: "call", func: 0, args: [1], keywords: [] }]);
    const cap = computeDeepBytes(values) + 32 + (name === "sum" ? 0 : 32);
    expect(run(input, { maxTotalBytes: cap }, { values }, adapter)).toEqual(intVal(1n));
    pyError(() => run(input, { maxTotalBytes: cap - 1 }, { values }, adapter), "LimitExceeded", `evaluation memory budget of ${cap - 1} bytes exceeded`);
  });
}

for (const adapter of [false, true]) for (const name of ["min", "max"]) {
  /// (partial: string extrema charge yielded characters and the selected result)
  test(`R2 ${name} string iteration has exact active-budget boundaries (adapter=${adapter})`, () => {
    const input = call(name, "éa");
    const expected = strVal(name === "min" ? "a" : "é");
    const cap = computeDeepBytes(strVal("éa")) + computeDeepBytes(strVal("é"))
      + computeDeepBytes(strVal("a")) + computeDeepBytes(expected);
    expect(run(input, { maxTotalBytes: cap }, {}, adapter)).toEqual(expected);
    pyError(() => run(input, { maxTotalBytes: cap - 1 }, {}, adapter), "LimitExceeded",
      `evaluation memory budget of ${cap - 1} bytes exceeded`);
  });
}

/// (partial: context leaves are snapshotted at each reference across trusted callbacks)
test("N4 a trusted callback may publish a changed context leaf between references", () => {
  const leaf = { kind: "str" as const, value: "before" };
  const mutate = callableVal("mutate", () => { leaf.value = "after"; return NONE; });
  const input = ast([
    { kind: "name", name: "context" }, { kind: "literal", value: { kind: "str", value: "x" } },
    { kind: "subscript", value: 0, slice: { kind: "index", value: 1 } },
    { kind: "name", name: "mutate" }, { kind: "call", func: 3, args: [], keywords: [] },
    { kind: "name", name: "context" },
    { kind: "literal", value: { kind: "str", value: "x" } },
    { kind: "subscript", value: 5, slice: { kind: "index", value: 6 } },
    { kind: "tuple", items: [2, 4, 7] },
  ]);
  const handle = createRecordExpression(input, {
    id: false, record: { mutate }, session: { ...session(), context: { x: leaf } },
  });
  try {
    expect(handle.read).toEqual({ kind: "tuple", items: [strVal("before"), NONE, strVal("after")] });
  } finally { handle.dispose(); }
});

/// (partial: dictionary construction refuses surplus positional arguments)
test("N3 dict refuses surplus positional arguments", () => {
  const input = ast([{ kind: "name", name: "dict" }, { kind: "literal", value: { kind: "none" } },
    { kind: "literal", value: { kind: "none" } },
    { kind: "call", func: 0, args: [1, 2], keywords: [] }]);
  pyError(() => run(input, {}), "TypeError", "dict expected at most 1 argument, got 2");
});

/// (partial: produced bound capabilities include captured receiver size)
test("R3 bound method production enforces captured-receiver per-value bytes", () => {
  const x = strVal("a".repeat(32));
  const input = ast([{ kind: "name", name: "x" }, { kind: "attribute", value: 0, attr: "lower" }]);
  expect(run(input, { maxValueBytes: 96 }, { x }).kind).toBe("callable");
  pyError(() => run(input, { maxValueBytes: 95 }, { x }), "LimitExceeded", "value of 96 bytes exceeds the per-value limit of 95 bytes");
});

for (const receiver of ["dict", "Command"]) {
  /// (partial: captured metadata is bounded after binding dictionary and builtin-object methods)
  test(`R3 ${receiver} bound method includes captured receiver bytes`, () => {
    const d = dictVal([[strVal("x"), intVal(1n)]]);
    const name = receiver === "dict" ? "d" : "Command";
    const input = ast([{ kind: "name", name },
      { kind: "attribute", value: 0, attr: receiver === "dict" ? "keys" : "create" }]);
    const cap = receiver === "dict" ? 32 + computeDeepBytes(d) : 64;
    expect(run(input, { maxValueBytes: cap }, { d }).kind).toBe("callable");
    pyError(() => run(input, { maxValueBytes: cap - 1 }, { d }), "LimitExceeded",
      `value of ${cap} bytes exceeds the per-value limit of ${cap - 1} bytes`);
  });
}

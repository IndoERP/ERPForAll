/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Expression signal adapter tests (§7, §9).
// Integrates lazy expression evaluation with Elang record signals and disposal.

import { setFlagsFromString } from "node:v8";
import { runInNewContext } from "node:vm";
import { expect, test } from "vitest";
import {
  decodeAst,
  parseBoundedJson,
  intVal,
  boolVal,
  strVal,
  listVal,
  dictVal,
  callableVal,
  PyDict,
  NONE,
  TRUE,
  FALSE,
  PythonError,
  type ValidatedAst,
} from "@erpforall/expr";
import { signal, effect, flushSync, computed, onError, unmount, UIError } from "../src/index.js";
import { comp, mountApp } from "./harness.js";
import {
  createRecordExpression,
  DisposedExpression,
  type FieldResolver,
  type ParentResolver,
  type RecordResolver,
  type SessionBindings,
  type ExpressionHandle,
} from "../src/runtime/expr.js";

function makeAst(expr: any) {
  return decodeAst(parseBoundedJson(JSON.stringify(expr)));
}

/** Runs `fn` and returns what it threw; fails the test when it returns normally. */
function thrown(fn: () => unknown): unknown {
  try {
    fn();
  } catch (e) {
    return e;
  }
  throw new Error("expected the call to throw, it returned normally");
}

/** Asserts `fn` throws a PythonError of exactly this class and message. */
function expectPythonError(fn: () => unknown, className: string, detail: string): void {
  const e = thrown(fn);
  expect(e).toBeInstanceOf(PythonError);
  expect((e as PythonError).className).toBe(className);
  expect((e as PythonError).message).toBe(`${className}: ${detail}`);
}

/** Asserts `fn` throws a UIError with exactly this code and message. */
function expectUIError(fn: () => unknown, code: string, message: string): UIError {
  const e = thrown(fn);
  expect(e).toBeInstanceOf(UIError);
  expect((e as UIError).code).toBe(code);
  expect((e as UIError).message).toBe(message);
  return e as UIError;
}

const DISPOSED = "expression handle has been disposed";
const DISPOSED_DURING_EVAL = "expression handle was disposed during evaluation";

/** Asserts `fn` throws a DisposedExpression (code `disposed`) with exactly this message. */
function expectDisposed(fn: () => unknown, message: string): DisposedExpression {
  const e = expectUIError(fn, "disposed", message);
  expect(e).toBeInstanceOf(DisposedExpression);
  expect(e.name).toBe("DisposedExpression");
  return e as DisposedExpression;
}

function defaultSession(overrides: Partial<SessionBindings> = {}): SessionBindings {
  return {
    uid: 1001n,
    allowed_company_ids: [1n, 2n],
    current_company_id: 1n,
    context: signal(new Map()),
    ...overrides,
  };
}

/// (partial: memoized unchanged reads and lazy record signal access)
test("memoized unchanged reads reuse cached result without reevaluation", () => {
  let accessCount = 0;
  const amountSig = signal(intVal(100n));

  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "amount") {
        accessCount++;
        return amountSig();
      }
      return undefined;
    },
  };

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "amount" },
      { kind: "literal", value: { kind: "int", value: "10" } },
      { kind: "binary", op: "add", left: 0, right: 1 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 42n,
    record: resolver,
    session: defaultSession(),
  });

  expect(accessCount).toBe(0);

  // First read evaluates
  const v1 = handle.read;
  expect(v1).toEqual(intVal(110n));
  expect(accessCount).toBe(1);

  // Second and third reads reuse cached result (zero resolver accesses)
  const v2 = handle.read;
  const v3 = handle.read;
  expect(v2).toEqual(intVal(110n));
  expect(v3).toEqual(intVal(110n));
  expect(accessCount).toBe(1);

  // Changing observed signal causes reevaluation on next read
  amountSig.set(intVal(200n));
  expect(accessCount).toBe(1); // lazy: not yet read

  const v4 = handle.read;
  expect(v4).toEqual(intVal(210n));
  expect(accessCount).toBe(2);

  handle.dispose();
});

/// (partial: branch-dependent signal subscriptions)
test("branch-dependent subscriptions track only active branches", () => {
  let activeAccesses = 0;
  let expensiveAccesses = 0;
  let fallbackAccesses = 0;

  const activeSig = signal(boolVal(false));
  const expensiveSig = signal(intVal(999n));
  const fallbackSig = signal(intVal(10n));

  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "active") {
        activeAccesses++;
        return activeSig();
      }
      if (name === "expensive") {
        expensiveAccesses++;
        return expensiveSig();
      }
      if (name === "fallback") {
        fallbackAccesses++;
        return fallbackSig();
      }
      return undefined;
    },
  };

  // expensive if active else fallback
  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "active" },
      { kind: "name", name: "expensive" },
      { kind: "name", name: "fallback" },
      { kind: "if_exp", test: 0, body: 1, orelse: 2 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: resolver,
    session: defaultSession(),
  });

  // Initially active is false -> reads active and fallback, NOT expensive
  expect(handle.read).toEqual(intVal(10n));
  expect(activeAccesses).toBe(1);
  expect(fallbackAccesses).toBe(1);
  expect(expensiveAccesses).toBe(0);

  // Modifying expensiveSig must NOT invalidate or reevaluate since it was not in active branch
  expensiveSig.set(intVal(1000n));
  expect(handle.read).toEqual(intVal(10n));
  expect(expensiveAccesses).toBe(0);

  // Switch active to true -> now reads active and expensive
  activeSig.set(boolVal(true));
  expect(handle.read).toEqual(intVal(1000n));
  expect(expensiveAccesses).toBe(1);

  // Now modifying fallbackSig must NOT invalidate or reevaluate
  fallbackSig.set(intVal(20n));
  expect(handle.read).toEqual(intVal(1000n));
  expect(fallbackAccesses).toBe(1); // Still 1 from initial read

  handle.dispose();
});

/// (partial: lazy parent attribute signals and parent identity)
test("lazy parent attribute signals and parent identity tracking", () => {
  let parentFieldAccesses = 0;
  const parentAmountSig = signal(intVal(500n));

  const parentResolver: RecordResolver = {
    getField(name: string) {
      if (name === "amount") {
        parentFieldAccesses++;
        return parentAmountSig();
      }
      return undefined;
    },
  };

  const parentSig = signal<ParentResolver | null>(parentResolver);

  // parent.amount + 50
  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "parent" },
      { kind: "attribute", value: 0, attr: "amount" },
      { kind: "literal", value: { kind: "int", value: "50" } },
      { kind: "binary", op: "add", left: 1, right: 2 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 10n,
    record: { getField: () => undefined },
    parent: parentSig,
    session: defaultSession(),
  });

  expect(handle.read).toEqual(intVal(550n));
  expect(parentFieldAccesses).toBe(1);

  // Changing parent amount signal reevaluates
  parentAmountSig.set(intVal(600n));
  expect(handle.read).toEqual(intVal(650n));
  expect(parentFieldAccesses).toBe(2);

  // Changing parent identity to new parent
  const parent2AmountSig = signal(intVal(1000n));
  const parent2Resolver: RecordResolver = {
    getField(name: string) {
      if (name === "amount") return parent2AmountSig();
      return undefined;
    },
  };
  parentSig.set(parent2Resolver);
  expect(handle.read).toEqual(intVal(1050n));

  // If parent is null -> NameError
  parentSig.set(null);
  expectPythonError(() => handle.read, "NameError", "name 'parent' is not defined");

  handle.dispose();
});

/// (partial: context, uid, company session bindings)
test("session bindings provide id, uid, allowed_company_ids, current_company_id, and context", () => {
  const contextSig = signal(new Map([["lang", strVal("fr_FR")]]));

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "uid" },
      { kind: "name", name: "id" },
      { kind: "binary", op: "add", left: 0, right: 1 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 5n,
    record: { getField: () => undefined },
    session: defaultSession({
      uid: 100n,
      context: contextSig,
    }),
  });

  expect(handle.read).toEqual(intVal(105n));
  handle.dispose();

  // Test unsaved record where id is False
  const astIdBool = makeAst({
    schema: 1,
    nodes: [{ kind: "name", name: "id" }],
  });
  const handleUnsaved = createRecordExpression(astIdBool, {
    id: false,
    session: defaultSession(),
  });
  expect(handleUnsaved.read).toEqual(FALSE);
  handleUnsaved.dispose();
});

/// (partial: session validation fails loudly)
test("session validation fails loudly on missing or malformed inputs", () => {
  const ast = makeAst({
    schema: 1,
    nodes: [{ kind: "literal", value: { kind: "int", value: "1" } }],
  });

  // Missing session
  expectUIError(
    () => createRecordExpression(ast, { id: 1n } as any),
    "value",
    "createRecordExpression requires session bindings",
  );

  // Invalid uid (non-integer string)
  expectUIError(
    () => createRecordExpression(ast, { id: 1n, session: defaultSession({ uid: "admin" as any }) }),
    "value",
    "session.uid must be an integer, got string",
  );

  // Invalid id (true instead of false or integer)
  expectUIError(
    () => createRecordExpression(ast, { id: true as any, session: defaultSession() }),
    "value",
    "unsaved record id must be False, not True",
  );

  // Collision with reserved context name in record fields
  expectUIError(
    () =>
      createRecordExpression(ast, {
        id: 1n,
        record: {
          getField: (name) => (name === "uid" ? intVal(999n) : undefined),
          hasField: (name) => name === "uid",
        },
        session: defaultSession(),
      }),
    "value",
    "reserved context name 'uid' cannot be overwritten by record field",
  );
});

/// (partial: fresh budget per evaluation)
test("evaluations use fresh budgets and reset step counts", () => {
  const counterSig = signal(intVal(1n));
  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "x") return counterSig();
      return undefined;
    },
  };

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "x" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "binary", op: "add", left: 0, right: 1 },
    ],
  });

  // Set tight step limit of 20
  const handle = createRecordExpression(ast, {
    id: 1n,
    record: resolver,
    session: defaultSession(),
    limits: { maxSteps: 20 },
  });

  // Run 30 consecutive evaluations across signal changes.
  // If budget were shared, 30 * steps would exceed 20 and fail.
  for (let i = 1n; i <= 30n; i++) {
    counterSig.set(intVal(i));
    expect(handle.read).toEqual(intVal(i + 1n));
  }

  handle.dispose();
});

/// (partial: error recovery on dependency change)
test("error recovery propagates exception then recovers when signal changes", () => {
  const denomSig = signal(intVal(0n));
  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "d") return denomSig();
      return undefined;
    },
  };

  // 100 // d
  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "100" } },
      { kind: "name", name: "d" },
      { kind: "binary", op: "floor_div", left: 0, right: 1 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: resolver,
    session: defaultSession(),
  });

  // Division by zero throws ZeroDivisionError
  expectPythonError(() => handle.read, "ZeroDivisionError", "integer division or modulo by zero");

  // Changing denominator to 5 recovers
  denomSig.set(intVal(5n));
  expect(handle.read).toEqual(intVal(20n));

  handle.dispose();
});

/// (partial: record isolation)
test("record isolation ensures independent records never share state", () => {
  const r1Sig = signal(intVal(10n));
  const r2Sig = signal(intVal(20n));

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "val" },
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "binary", op: "add", left: 0, right: 1 },
    ],
  });

  const h1 = createRecordExpression(ast, {
    id: 1n,
    record: { getField: (n) => (n === "val" ? r1Sig() : undefined) },
    session: defaultSession(),
  });

  const h2 = createRecordExpression(ast, {
    id: 2n,
    record: { getField: (n) => (n === "val" ? r2Sig() : undefined) },
    session: defaultSession(),
  });

  expect(h1.read).toEqual(intVal(11n));
  expect(h2.read).toEqual(intVal(21n));

  r1Sig.set(intVal(100n));
  expect(h1.read).toEqual(intVal(101n));
  expect(h2.read).toEqual(intVal(21n)); // h2 untouched

  h1.dispose();
  h2.dispose();
});

/// (partial: explicit disposal releases dependencies and prevents reads)
test("explicit disposal invalidates dependencies and subsequent reads throw DisposedExpression", () => {
  let accessCount = 0;
  const s = signal(intVal(1n));
  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "x") {
        accessCount++;
        return s();
      }
      return undefined;
    },
  };

  const ast = makeAst({
    schema: 1,
    nodes: [{ kind: "name", name: "x" }],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: resolver,
    session: defaultSession(),
  });

  expect(handle.read).toEqual(intVal(1n));
  expect(accessCount).toBe(1);

  handle.dispose();
  expect(handle.isDisposed).toBe(true);

  // Calling dispose again is idempotent
  handle.dispose();

  // Subsequent reads throw DisposedExpression
  expectDisposed(() => handle.read, DISPOSED);

  // Writing to signal does not trigger any reevaluation
  s.set(intVal(99n));
  expect(accessCount).toBe(1);
});

/// (partial: live consumer effect receives DisposedExpression on flush)
test("live observing effect receives DisposedExpression on flush after disposal", async () => {
  const s = signal(intVal(1n));
  const ast = makeAst({
    schema: 1,
    nodes: [{ kind: "name", name: "x" }],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: { getField: (n) => (n === "x" ? s() : undefined) },
    session: defaultSession(),
  });

  const errors: unknown[] = [];
  const { C, t } = comp("TestConsumer", () => null, {
    setup: () => {
      onError((e) => {
        errors.push(e);
      });
      // Effect observing handle.read
      effect(() => {
        handle.read;
      });
    },
  });

  const { root } = await mountApp(C, [t], { onError: (e) => errors.push(e) });
  // App mounted, initial effect ran successfully
  expect(errors).toHaveLength(0);

  // Dispose handle
  handle.dispose();

  // Flush scheduler
  flushSync();

  // The live effect flushes, reads handle.read, and receives DisposedExpression
  expect(errors).toHaveLength(1);
  expect(errors[0]).toBeInstanceOf(DisposedExpression);
  expect((errors[0] as DisposedExpression).code).toBe("disposed");
  expect((errors[0] as Error).message).toBe(DISPOSED);

  // Clean up
  unmount(root);
});

/// (partial: reentrant disposal during evaluation)
test("reentrant disposal during evaluation discards in-flight result and throws DisposedExpression", () => {
  let handleRef: ExpressionHandle | null = null;
  const resolver: FieldResolver = {
    getField(name: string) {
      if (name === "self_destruct") {
        handleRef?.dispose();
        return intVal(42n);
      }
      return undefined;
    },
  };

  const ast = makeAst({
    schema: 1,
    nodes: [{ kind: "name", name: "self_destruct" }],
  });

  handleRef = createRecordExpression(ast, {
    id: 1n,
    record: resolver,
    session: defaultSession(),
  });

  expectDisposed(() => handleRef!.read, DISPOSED_DURING_EVAL);
  expect(handleRef!.isDisposed).toBe(true);
  expectDisposed(() => handleRef!.read, DISPOSED);
});

/// (partial: owner teardown auto-disposal)
test("owner teardown automatically disposes expression handle", async () => {
  let handle: ExpressionHandle | null = null;
  const s = signal(intVal(1n));

  const { C, t } = comp("OwnerComp", () => null, {
    setup: () => {
      const ast = makeAst({
        schema: 1,
        nodes: [{ kind: "name", name: "x" }],
      });
      handle = createRecordExpression(ast, {
        id: 1n,
        record: { getField: (n) => (n === "x" ? s() : undefined) },
        session: defaultSession(),
      });
    },
  });

  const { root } = await mountApp(C, [t]);
  expect(handle).not.toBeNull();
  expect(handle!.isDisposed).toBe(false);
  expect(handle!.read).toEqual(intVal(1n));

  // Destroy component / app
  unmount(root);

  expect(handle!.isDisposed).toBe(true);
  expectDisposed(() => handle!.read, DISPOSED);
});

/// (partial: bare-read-then-dispose and failed-read-then-dispose)
test("bare-read-then-dispose and failed-read-then-dispose detach dependencies", () => {
  // 1. Bare read then dispose
  let bareAccesses = 0;
  const bareSig = signal(intVal(10n));
  const bareAst = makeAst({
    schema: 1,
    nodes: [{ kind: "name", name: "bare" }],
  });
  const bareHandle = createRecordExpression(bareAst, {
    id: 1n,
    record: {
      getField(n) {
        if (n === "bare") {
          bareAccesses++;
          return bareSig();
        }
        return undefined;
      },
    },
    session: defaultSession(),
  });

  expect(bareHandle.read).toEqual(intVal(10n));
  expect(bareAccesses).toBe(1);

  bareHandle.dispose();
  expect(bareHandle.isDisposed).toBe(true);

  // Updating signal after bare-read disposal must not trigger any evaluation
  bareSig.set(intVal(999n));
  expect(bareAccesses).toBe(1);
  expectDisposed(() => bareHandle.read, DISPOSED);

  // 2. Failed read then dispose
  let failedAccesses = 0;
  const zeroSig = signal(intVal(0n));
  const failAst = makeAst({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "name", name: "z" },
      { kind: "binary", op: "floor_div", left: 0, right: 1 },
    ],
  });
  const failHandle = createRecordExpression(failAst, {
    id: 2n,
    record: {
      getField(n) {
        if (n === "z") {
          failedAccesses++;
          return zeroSig();
        }
        return undefined;
      },
    },
    session: defaultSession(),
  });

  expectPythonError(() => failHandle.read, "ZeroDivisionError", "integer division or modulo by zero");
  expect(failedAccesses).toBe(1);

  failHandle.dispose();
  expect(failHandle.isDisposed).toBe(true);

  // Updating signal after failed-read disposal must not trigger reevaluation
  zeroSig.set(intVal(10n));
  expect(failedAccesses).toBe(1);
  expectDisposed(() => failHandle.read, DISPOSED);
});

/// (partial: context signal updates and dictionary of signals)
test("record dictionary of signals and context signal updates", () => {
  const amountSig = signal(intVal(42n));
  const contextSig = signal(new Map([["currency", strVal("USD")]]));

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "amount" },
      { kind: "name", name: "context" },
      { kind: "literal", value: { kind: "str", value: "currency" } },
      { kind: "subscript", value: 1, slice: { kind: "index", value: 2 } },
      { kind: "tuple", items: [0, 3] },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: {
      amount: amountSig,
    },
    session: defaultSession({
      context: contextSig,
    }),
  });

  const r1 = handle.read as any;
  expect(r1.kind).toBe("tuple");
  expect(r1.items[0]).toEqual(intVal(42n));
  expect(r1.items[1]).toEqual(strVal("USD"));

  // Updating context signal reevaluates
  contextSig.set(new Map([["currency", strVal("IDR")]]));
  const r2 = handle.read as any;
  expect(r2.items[1]).toEqual(strVal("IDR"));

  handle.dispose();
});

/// (partial: unchanged cached read zero resolver calls)
test("unchanged cached read incurs zero resolver calls across repeated reads", () => {
  let resolverCalls = 0;
  const numSig = signal(intVal(7n));

  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "n" },
      { kind: "literal", value: { kind: "int", value: "3" } },
      { kind: "binary", op: "mult", left: 0, right: 1 },
    ],
  });

  const handle = createRecordExpression(ast, {
    id: 1n,
    record: {
      getField(n) {
        if (n === "n") {
          resolverCalls++;
          return numSig();
        }
        return undefined;
      },
    },
    session: defaultSession(),
  });

  // Perform 100 consecutive reads
  for (let i = 0; i < 100; i++) {
    expect(handle.read).toEqual(intVal(21n));
  }
  // Exactly 1 initial resolver call; all other 99 reads are completely cached!
  expect(resolverCalls).toBe(1);

  handle.dispose();
});


// --- pre-merge delta check: D1-D4, S2-S4 -------------------

/** `func(arg)` where both are names. */
function callAst(func: string, arg: string): ValidatedAst {
  return makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: func },
      { kind: "name", name: arg },
      { kind: "call", func: 0, args: [1], keywords: [] },
    ],
  });
}

// Review D1: standard builtins reachable through the record adapter (no/052 behaviour).
test("builtin calls evaluate through createRecordExpression", () => {
  const lines = signal(listVal([intVal(1n), intVal(2n), intVal(3n)]));
  const partner = signal<ReturnType<typeof intVal> | typeof FALSE>(FALSE);
  const record: RecordResolver = {
    getField: (n) => (n === "line_ids" ? lines() : n === "partner_id" ? partner() : undefined),
  };

  const len = createRecordExpression(callAst("len", "line_ids"), { id: 1n, record, session: defaultSession(), owner: null });
  expect(len.read).toEqual(intVal(3n));
  lines.set(listVal([intVal(7n)]));
  expect(len.read).toEqual(intVal(1n));

  const truthy = createRecordExpression(callAst("bool", "partner_id"), { id: 1n, record, session: defaultSession(), owner: null });
  expect(truthy.read).toEqual(FALSE);
  partner.set(intVal(9n));
  expect(truthy.read).toEqual(TRUE);

  // A name that is neither reserved, a field nor a builtin is still a NameError.
  const missing = createRecordExpression(callAst("len", "no_such_field"), { id: 1n, record, session: defaultSession(), owner: null });
  expectPythonError(() => missing.read, "NameError", "name 'no_such_field' is not defined");

  len.dispose();
  truthy.dispose();
  missing.dispose();
});

test("a record field named like a builtin reads the field", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "date" }] });
  const h = createRecordExpression(ast, {
    id: 1n,
    record: { getField: (n) => (n === "date" ? strVal("2026-10-25") : undefined) },
    session: defaultSession(),
    owner: null,
  });
  expect(h.read).toEqual(strVal("2026-10-25"));
  h.dispose();
});

// Review N4: a declared field without a value fails loudly instead of reading a builtin of the same name.
test("a declared field without a value does not fall back to a builtin", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "date" }] });
  const h = createRecordExpression(ast, {
    id: 1n,
    record: { getField: () => undefined, hasField: (n) => n === "date" },
    session: defaultSession(),
    owner: null,
  });
  expectUIError(() => h.read, "value", "record field 'date' is declared but has no value");
  h.dispose();
});

// Review D3: context values must be expression data values.
test("context values that are not expression data values fail loudly naming the key", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "context" }] });
  const read = (context: unknown) => {
    const h = createRecordExpression(ast, {
      id: 1n,
      session: defaultSession({ context: context as SessionBindings["context"] }),
      owner: null,
    });
    try {
      return h.read;
    } finally {
      h.dispose();
    }
  };
  const kinds = "none, bool, int, float, str, bytes, list, tuple or dict";

  // Chosen behaviour: JS primitives are refused, not converted.
  expectUIError(() => read({ lang: "en_US" }), "value", `session.context['lang'] must be an expression value (${kinds}), got JS string`);
  expectUIError(() => read(new Map([["tz", 7]])), "value", `session.context['tz'] must be an expression value (${kinds}), got JS number`);
  expectUIError(() => read(signal(new Map([["active", true]]))), "value", `session.context['active'] must be an expression value (${kinds}), got JS boolean`);
  expectUIError(
    () => read({ kind: "dict", entries: [[strVal("ids"), { kind: "list", items: [intVal(1n), 2] }]] }),
    "value",
    `session.context['ids'][1] must be an expression value (${kinds}), got JS number`,
  );
  expectUIError(
    () => read({ fn: callableVal("f", () => NONE) }),
    "value",
    `session.context['fn'] must be an expression value (${kinds}), got kind 'callable'`,
  );
  expectUIError(
    () => read({ forged: { kind: "int", value: 3 } }),
    "value",
    `session.context['forged'] must be an expression value (${kinds}), got kind 'int'`,
  );
  expectUIError(
    () => read(dictVal([[listVal([]), NONE]])),
    "value",
    "session.context key <list> is unhashable (kind 'list')",
  );
  expectUIError(
    () => read(dictVal([[strVal("a"), NONE], [strVal("a"), TRUE]])),
    "value",
    "session.context has a duplicate key 'a'",
  );
  expectUIError(() => read(["lang"]), "value", "invalid context binding: expected a dict or mapping, got JS array");

  // Valid data values of every mapping form are admitted.
  const nested = dictVal([[strVal("ids"), listVal([intVal(1n)])]]);
  const expected = dictVal([[strVal("lang"), strVal("en_US")], [strVal("nested"), nested]]);
  expect(read({ lang: strVal("en_US"), nested })).toEqual(expected);
  expect(read(new Map<string, any>([["lang", strVal("en_US")], ["nested", nested]]))).toEqual(expected);
  expect(read(new PyDict(expected.entries))).toEqual(expected);
  expect(read(expected)).toEqual(expected);
});

// Review D3: context validation is bounded by the evaluation limits.
test("context validation is bounded by depth and value count", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "context" }] });
  const read = (context: unknown, limits: { maxDepth?: number; maxSteps?: number }) => {
    const h = createRecordExpression(ast, {
      id: 1n,
      session: defaultSession({ context: context as SessionBindings["context"] }),
      limits,
      owner: null,
    });
    try {
      return h.read;
    } finally {
      h.dispose();
    }
  };
  // A self-referencing list would never end without the depth bound.
  const cyclic: { kind: "list"; items: unknown[] } = { kind: "list", items: [] };
  cyclic.items.push(cyclic);
  expectUIError(() => read({ loop: cyclic }, { maxDepth: 5 }), "value", "session.context['loop'][0][0][0][0][0] nests deeper than 5 levels");
  expectUIError(
    () => read({ many: listVal(Array.from({ length: 10 }, (_, i) => intVal(BigInt(i)))) }, { maxSteps: 5 }),
    "value",
    "session.context holds more than 5 values",
  );
});

// Integration review I1: host collections are checked before copying and holes are invalid.
function readAdmissionContext(context: unknown, maxSteps = 100) {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "context" }] });
  const h = createRecordExpression(ast, {
    id: 1n,
    session: defaultSession({ context: context as SessionBindings["context"] }),
    limits: { maxSteps },
    owner: null,
  });
  try {
    return h.read;
  } finally {
    h.dispose();
  }
}

test.each(["list", "tuple"])("context %s rejects holes with a typed path error", (kind) => {
  const items = new Array(2);
  items[0] = NONE;
  expectUIError(
    () => readAdmissionContext({ ids: { kind, items } }),
    "value",
    "session.context['ids'][1] must be an expression value (none, bool, int, float, str, bytes, list, tuple or dict), got JS undefined",
  );
});

test.each(["list", "tuple"])("context %s checks remaining capacity before reading members", (kind) => {
  const items = new Array(5);
  let reads = 0;
  Object.defineProperty(items, 0, { get() { reads++; return NONE; } });
  expectUIError(
    () => readAdmissionContext({ ids: { kind, items } }, 5),
    "value", "session.context holds more than 5 values",
  );
  expect(reads).toBe(0);
});

test("context Map refuses oversized input before requesting its entries", () => {
  class WatchedMap extends Map<string, typeof NONE> {
    entryReads = 0;
    override entries() { this.entryReads++; return super.entries(); }
  }
  const context = new WatchedMap(Array.from({ length: 6 }, (_, i) => [String(i), NONE]));
  expectUIError(() => readAdmissionContext(context, 5), "value", "session.context holds more than 5 values");
  expect(context.entryReads).toBe(0);
});

test("context object refuses oversized input before reading values", () => {
  let reads = 0;
  const context = Object.create(null) as Record<string, unknown>;
  for (let i = 0; i < 6; i++) {
    Object.defineProperty(context, String(i), { enumerable: true, get() { reads++; return NONE; } });
  }
  expectUIError(() => readAdmissionContext(context, 5), "value", "session.context holds more than 5 values");
  expect(reads).toBe(0);
});

/// (partial: dispose during evaluation detaches dependencies, review D2)
test("disposal during evaluation detaches every dependency read before it", () => {
  let handleRef: ExpressionHandle | null = null;
  let amountReads = 0;
  const amount = signal(intVal(1n));
  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "name", name: "amount" },
      { kind: "name", name: "self_destruct" },
      { kind: "binary", op: "add", left: 0, right: 1 },
    ],
  });
  handleRef = createRecordExpression(ast, {
    id: 1n,
    record: {
      getField(n) {
        if (n === "amount") {
          amountReads++;
          return amount();
        }
        if (n === "self_destruct") {
          handleRef!.dispose();
          return intVal(1n);
        }
        return undefined;
      },
    },
    session: defaultSession(),
    owner: null,
  });

  // A consumer that read the handle before: its computation must not be re-run by the field signal.
  let consumerRuns = 0;
  const consumer = computed(() => {
    consumerRuns++;
    return thrown(() => handleRef!.read);
  });
  const first = consumer() as DisposedExpression;
  expect(first).toBeInstanceOf(DisposedExpression);
  expect(first.message).toBe(DISPOSED_DURING_EVAL);
  expect(amountReads).toBe(1);

  amount.set(intVal(2n));
  flushSync();
  // Still cached: the field signal no longer reaches the consumer through the disposed computation.
  expect(consumer()).toBe(first);
  expect(consumerRuns).toBe(1);
  expect(amountReads).toBe(1);
  expectDisposed(() => handleRef!.read, DISPOSED);
  expect(amountReads).toBe(1);
});

/// (partial: a disposed handle releases its AST and record, review D2)
test("a disposed handle releases its AST and record resolver; a live one keeps them", async () => {
  setFlagsFromString("--expose-gc");
  const gc = runInNewContext("gc") as () => void;

  function build(selfDestruct: boolean) {
    const box: { handle: ExpressionHandle | null } = { handle: null };
    const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "x" }] });
    const record: RecordResolver = {
      getField(n) {
        if (n !== "x") return undefined;
        if (selfDestruct) box.handle!.dispose();
        return intVal(1n);
      },
    };
    box.handle = createRecordExpression(ast, { id: 1n, record, session: defaultSession(), owner: null });
    return { box, ast: new WeakRef(ast), record: new WeakRef(record) };
  }

  const kept = build(false);
  expect(kept.box.handle!.read).toEqual(intVal(1n));

  const explicit = build(false);
  expect(explicit.box.handle!.read).toEqual(intVal(1n));
  explicit.box.handle!.dispose();

  const during = build(true);
  expectDisposed(() => during.box.handle!.read, DISPOSED_DURING_EVAL);

  // WeakRef targets stay alive until the current job ends.
  await new Promise((resolve) => setTimeout(resolve, 0));
  gc();

  expect(kept.ast.deref()).toBeDefined();
  expect(kept.record.deref()).toBeDefined();
  expect(explicit.ast.deref()).toBeUndefined();
  expect(explicit.record.deref()).toBeUndefined();
  expect(during.ast.deref()).toBeUndefined();
  expect(during.record.deref()).toBeUndefined();
  // The handles themselves are still reachable here.
  expect(kept.box.handle!.read).toEqual(intVal(1n));
  expect(explicit.box.handle!.isDisposed).toBe(true);
  expect(during.box.handle!.isDisposed).toBe(true);
});

/// (partial: an evaluation error after disposal is kept as the cause, review D2)
test("an evaluation that fails after disposal reports the disposal with the failure as cause", () => {
  let handleRef: ExpressionHandle | null = null;
  // 1 // zero, where reading `zero` disposes the handle
  const ast = makeAst({
    schema: 1,
    nodes: [
      { kind: "literal", value: { kind: "int", value: "1" } },
      { kind: "name", name: "zero" },
      { kind: "binary", op: "floor_div", left: 0, right: 1 },
    ],
  });
  handleRef = createRecordExpression(ast, {
    id: 1n,
    record: {
      getField(n) {
        if (n !== "zero") return undefined;
        handleRef!.dispose();
        return intVal(0n);
      },
    },
    session: defaultSession(),
    owner: null,
  });
  const e = expectDisposed(() => handleRef!.read, DISPOSED_DURING_EVAL);
  expect(e.cause).toBeInstanceOf(PythonError);
  expect((e.cause as PythonError).message).toBe("ZeroDivisionError: integer division or modulo by zero");
});

/// (partial: building a handle does not subscribe the surrounding computation, review S2)
test("building a handle inside a computation does not subscribe it to field signals", () => {
  const version = signal(0);
  const record: RecordResolver = {
    getField(n) {
      version(); // a resolver that reads a signal for every name, the reserved ones included
      return n === "x" ? intVal(1n) : undefined;
    },
  };
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "x" }] });
  let runs = 0;
  const handles: ExpressionHandle[] = [];
  const builder = computed(() => {
    runs++;
    handles.push(createRecordExpression(ast, { id: 1n, record, session: defaultSession(), owner: null }));
    return runs;
  });
  expect(builder()).toBe(1);
  version.set(1);
  expect(builder()).toBe(1);
  expect(runs).toBe(1);
  for (const h of handles) h.dispose();
});

// Review S3: only decoder-admitted ASTs are accepted.
test("a hand-built AST object is refused", () => {
  const forged = { nodes: [{ kind: "name", name: "x" }], height: 1 } as unknown as ValidatedAst;
  expectUIError(
    () => createRecordExpression(forged, { id: 1n, session: defaultSession(), owner: null }),
    "value",
    "createRecordExpression requires a ValidatedAst returned by decodeAst",
  );
});

// Review S4: DisposedExpression is a typed UIError.
test("DisposedExpression is a UIError with the code 'disposed'", () => {
  const e = new DisposedExpression("m");
  expect(e).toBeInstanceOf(UIError);
  expect(e.code).toBe("disposed");
  expect(e.name).toBe("DisposedExpression");
  const make = UIError as unknown as new (message: string, code: string) => UIError;
  expectUIError(
    () => new make("x", "disposed"),
    "usage",
    'UIError: the code "disposed" belongs to its own error class; use one of the base codes',
  );
});

/// (partial: context data is checked against byte budget before copying payloads)
test("context admission charges bytes before accessing a bytes payload", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "context" }] });
  let reads = 0;
  const raw = { kind: "bytes", get value() { reads++; return new Uint8Array(100); } };
  const handle = createRecordExpression(ast, {
    id: false, session: defaultSession({ context: { x: raw } as any }),
    limits: { maxTotalBytes: 32 },
  });
  expectPythonError(() => handle.read, "LimitExceeded", "evaluation memory budget of 32 bytes exceeded");
  expect(reads).toBe(0);
  handle.dispose();
});

/// (partial: context source validation happens once per evaluation)
test("repeated context names read the mapping once per evaluation", () => {
  const ast = makeAst({ schema: 1, nodes: [
    { kind: "name", name: "context" }, { kind: "name", name: "context" },
    { kind: "compare", left: 0, ops: ["eq"], comparators: [1] },
  ] });
  let reads = 0;
  const context = () => { reads++; return { x: intVal(1n) }; };
  const handle = createRecordExpression(ast, { id: false, session: defaultSession({ context }) });
  expect(handle.read).toEqual(TRUE);
  expect(reads).toBe(1);
  handle.dispose();
});

/// (partial: builtin lookup has the same admission budget through both entrypoints)
test("adapter builtins do not charge a host admission step", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "len" }] });
  const handle = createRecordExpression(ast, { id: false, session: defaultSession(), limits: { maxSteps: 1, maxTotalBytes: 1 } });
  expect(handle.read.kind).toBe("callable");
  handle.dispose();
});

/// (partial: forged typed session identifiers are refused)
test("session identifiers cannot smuggle a wrong bigint payload", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "uid" }] });
  expectUIError(() => createRecordExpression(ast, {
    id: false, session: defaultSession({ uid: { kind: "int", value: "1" } as any }),
  }), "value", "session.uid must be an integer, got invalid int payload");
});

/// (partial: primitive key admission does not spend an unbounded comparison walk)
test("numeric context keys are indexed within the admission work budget", () => {
  const mapping = new Map(Array.from({ length: 100 }, (_, i) => [intVal(i), NONE]));
  const result = readAdmissionContext(mapping, 300);
  expect(result.kind).toBe("dict");
  if (result.kind !== "dict") throw new Error("context is not a dictionary");
  expect(result.entries.length).toBe(100);
});

/// (partial: session identifier tags are data, never getters or truthy defaults)
test("session identifier validation does not execute a tag getter or coerce false", () => {
  const ast = makeAst({ schema: 1, nodes: [{ kind: "name", name: "uid" }] });
  let reads = 0;
  const uid = { get kind() { reads++; return "int"; }, value: 1n };
  expectUIError(() => createRecordExpression(ast, { id: false, session: defaultSession({ uid: uid as any }) }),
    "value", "session.uid must be an integer, got object");
  expect(reads).toBe(0);
  expectUIError(() => createRecordExpression(ast, { id: { kind: "bool", value: 0 } as any, session: defaultSession() }),
    "value", "unsaved record id must be False, got invalid bool payload");
});

/// (partial: native mapping storage is read without data getter/iterator overrides)
test("context mapping admission does not invoke host overrides", () => {
  let reads = 0;
  class WatchedMap extends Map<string, typeof NONE> {
    override get size() { reads++; return super.size; }
    override entries() { reads++; return super.entries(); }
  }
  class WatchedDict extends PyDict {
    override get length() { reads++; return super.length; }
    override get entries() { reads++; return super.entries; }
  }
  expect(readAdmissionContext(new WatchedMap([["x", NONE]]))).toEqual(dictVal([[strVal("x"), NONE]]));
  expect(reads).toBe(0);
  expect(readAdmissionContext(new WatchedDict([[strVal("x"), NONE]]))).toEqual(dictVal([[strVal("x"), NONE]]));
  expect(reads).toBe(0);
});

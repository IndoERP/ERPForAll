/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Expression signal adapter (§7, §9).
// Integrates lazy expression evaluation with Elang record signals and disposal.

import {
  type ValidatedAst,
  type ExprValue,
  type ExprObject,
  type DictValue,
  type IntValue,
  type BoolValue,
  type NoneValue,
  type EvalLimits,
  evalAst,
  createContext,
  isValidatedAst,
  isHashable,
  Budget,
  intVal,
  listVal,
  objectVal,
  NONE,
  FALSE,
  nameError,
  attributeError,
  PyDict,
  strVal,
  keysEqual,
  SLOT,
  EVAL_LIMITS_CEILING,
  computeDeepBytes,
  computeNodes,
} from "@erpforall/expr";
import { computed, signal, untrack, type Signal } from "./reactive.js";
import { type Owner, getOwner } from "./owner.js";
import { DisposedExpression, UIError } from "./errors.js";

export { DisposedExpression };

const I64_MIN = -9223372036854775808n;
const I64_MAX = 9223372036854775807n;

function checkI64(val: unknown, name: string): bigint {
  let b: bigint;
  if (typeof val === "bigint") {
    b = val;
  } else if (typeof val === "number") {
    if (!Number.isInteger(val)) {
      throw new UIError(`${name} must be an integer, got ${val}`, "value");
    }
    b = BigInt(val);
  } else if (typeof val === "object" && val !== null && Object.getOwnPropertyDescriptor(val, "kind")?.value === "int") {
    const payload = Object.getOwnPropertyDescriptor(val, "value");
    if (!payload || !("value" in payload) || typeof payload.value !== "bigint") {
      throw new UIError(`${name} must be an integer, got invalid int payload`, "value");
    }
    b = payload.value as bigint;
  } else {
    throw new UIError(`${name} must be an integer, got ${typeof val}`, "value");
  }
  if (b < I64_MIN || b > I64_MAX) {
    throw new UIError(`${name} out of i64 bounds: ${b}`, "value");
  }
  return b;
}

/** Interface for lazily resolving record field values. */
export interface RecordResolver {
  getField(name: string): ExprValue | undefined;
  hasField?(name: string): boolean;
}

export type FieldResolver =
  | RecordResolver
  | ((name: string) => ExprValue | undefined)
  | Record<string, Signal<ExprValue> | ExprValue | (() => ExprValue)>;

/**
 * A parent record given directly. The function form of `FieldResolver` is excluded: a function passed as
 * `parent` is always a parent source (a signal or getter returning the parent), so a function-style
 * resolver would be called without a field name. Wrap it as `{ getField }`.
 */
export type ParentResolver = Exclude<FieldResolver, (name: string) => ExprValue | undefined>;

type ContextMapping = PyDict | Record<string, ExprValue> | Map<string, ExprValue> | DictValue;

export interface SessionBindings {
  readonly uid: bigint | number | IntValue;
  readonly allowed_company_ids: readonly (bigint | number | IntValue)[];
  readonly current_company_id?: bigint | number | IntValue | null | NoneValue;
  /**
   * The view/action context. Every value must already be an expression data value (none, bool, int,
   * float, str, bytes, list, tuple or dict, nested); JS primitives are refused, not converted (UI1/FIELDS
   * own the conversions). Mapping keys may be JS strings (taken as `str`).
   */
  readonly context: Signal<ContextMapping> | (() => ContextMapping) | ContextMapping;
}

export interface RecordExpressionOptions {
  /** Resolver for record fields */
  readonly record?: FieldResolver;
  /** Persisted i64 record id, or false/FALSE for unsaved record */
  readonly id: bigint | number | IntValue | boolean | BoolValue;
  /** Enclosing x2many parent record, or signal/function returning parent, or null/undefined */
  readonly parent?:
    | ParentResolver
    | Signal<ParentResolver | null | undefined>
    | (() => ParentResolver | null | undefined)
    | null
    | undefined;
  /** Required session bindings */
  readonly session: SessionBindings;
  /** Optional limits to tighten evaluation ceilings */
  readonly limits?: Partial<EvalLimits>;
  /** Optional owner to attach cleanup to (defaults to getOwner()) */
  readonly owner?: Owner | null;
}

export interface ExpressionHandle {
  readonly read: ExprValue;
  dispose(): void;
  readonly isDisposed: boolean;
}

const RESERVED_NAMES = new Set([
  "id",
  "parent",
  "context",
  "uid",
  "allowed_company_ids",
  "current_company_id",
  "True",
  "False",
  "None",
]);

const ALLOWED_SESSION_KEYS = new Set([
  "uid",
  "allowed_company_ids",
  "current_company_id",
  "context",
]);

// Intrinsic storage access avoids invoking a subclass data getter or iterator override.
const mapSize = Object.getOwnPropertyDescriptor(Map.prototype, "size")!.get!;
const mapEntries = Map.prototype.entries;
const pyDictLength = Object.getOwnPropertyDescriptor(PyDict.prototype, "length")!.get!;
const pyDictEntries = Object.getOwnPropertyDescriptor(PyDict.prototype, "entries")!.get!;

const CONTEXT_KINDS = "none, bool, int, float, str, bytes, list, tuple or dict";

function toResolver(input: FieldResolver | undefined): RecordResolver {
  if (!input) return { getField: () => undefined };
  if (typeof input === "function") {
    return { getField: input as (name: string) => ExprValue | undefined };
  }
  if (
    typeof input === "object" &&
    input !== null &&
    "getField" in input &&
    typeof (input as RecordResolver).getField === "function"
  ) {
    return input as RecordResolver;
  }
  const obj = input as Record<string, unknown>;
  return {
    getField(name: string): ExprValue | undefined {
      if (!Object.hasOwn(obj, name)) return undefined;
      const v = obj[name];
      if (typeof v === "function") {
        return (v as () => ExprValue)();
      }
      return v as ExprValue;
    },
    hasField(name: string): boolean {
      return Object.hasOwn(obj, name);
    },
  };
}

/**
 * Refuses a record that binds a reserved name. Untracked: building a handle inside an effect or computed
 * must not subscribe that consumer to the record's field signals (review S2). A module function, so no
 * closure of the handle captures the record (a disposed handle must release it, review D2).
 */
function assertNoReservedFields(record: RecordResolver): void {
  untrack(() => {
    for (const resName of RESERVED_NAMES) {
      if (record.hasField?.(resName) || record.getField(resName) !== undefined) {
        throw new UIError(`reserved context name '${resName}' cannot be overwritten by record field`, "value");
      }
    }
  });
}

function describeHostValue(v: unknown): string {
  if (v === null) return "JS null";
  if (typeof v !== "object") return `JS ${typeof v}`;
  if (Array.isArray(v)) return "JS array";
  const kind = Object.getOwnPropertyDescriptor(v, "kind")?.value as unknown;
  if (typeof kind === "string") return `kind '${kind}'`;
  return "JS object";
}

/** Walk state of one context validation: values visited so far against the step ceiling. */
interface ContextWalk {
  values: number;
  readonly maxValues: number;
  readonly maxDepth: number;
  readonly budget: Budget;
  bytes: number;
}

/** Capacity reservation before allocating validation scratch; final data copying is Budget.admit's job. */
function reserveContextBytes(bytes: number, walk: ContextWalk): void {
  walk.bytes += bytes;
  walk.budget.checkSize(walk.bytes);
  walk.budget.checkAvailable(walk.bytes);
}
function contextData(value: object, key: PropertyKey, path: string): unknown {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  if (!descriptor || !("value" in descriptor)) {
    throw new UIError(`${path} must contain own data properties, not accessors`, "value");
  }
  return descriptor.value;
}

/** Reject a collection before allocating or visiting more members than can be admitted. */
function checkContextCapacity(count: number, walk: ContextWalk): void {
  if (count > walk.maxValues - walk.values) {
    throw new UIError(`session.context holds more than ${walk.maxValues} values`, "value");
  }
}

/**
 * Validates context data with path diagnostics and bounded normalization scratch. Payload snapshots
 * are copied only by the evaluator after Budget.admit charges them. The walk is bounded
 * by the evaluation limits: nesting by `maxDepth`, the number of values by `maxSteps` (each value would
 * cost at least one step when the evaluator admits the context).
 */
function admitContextValue(v: unknown, path: string, depth: number, walk: ContextWalk): ExprValue {
  if (depth > walk.maxDepth) {
    throw new UIError(`${path} nests deeper than ${walk.maxDepth} levels`, "value");
  }
  walk.values += 1;
  if (walk.values > walk.maxValues) {
    throw new UIError(`session.context holds more than ${walk.maxValues} values`, "value");
  }
  const refuse = (): never => {
    throw new UIError(`${path} must be an expression value (${CONTEXT_KINDS}), got ${describeHostValue(v)}`, "value");
  };
  if (typeof v !== "object" || v === null || Array.isArray(v)) return refuse();
  reserveContextBytes(SLOT, walk);
  const kind = contextData(v, "kind", path);
  if (typeof kind !== "string" || !["none", "bool", "int", "float", "str", "bytes", "list", "tuple", "dict"].includes(kind)) return refuse();
  // These reads use descriptors; none of the data getters may execute.
  const payload = kind === "none" ? undefined : contextData(v, kind === "list" || kind === "tuple" ? "items" : kind === "dict" ? "entries" : "value", path);
  switch (kind) {
    case "none":
      return NONE;
    case "bool":
      if (typeof payload !== "boolean") return refuse();
      return v as ExprValue;
    case "int":
      if (typeof payload !== "bigint") return refuse();
      walk.budget.checkValue(v as ExprValue); // validate i64 before indexing a key
      return v as ExprValue;
    case "float":
      if (typeof payload !== "number") return refuse();
      return v as ExprValue;
    case "str":
      if (typeof payload !== "string") return refuse();
      // Preflight the minimum size before any full UTF-8 metadata scan in Budget.admit.
      reserveContextBytes(payload.length, walk);
      return v as ExprValue;
    case "bytes":
      if (!(payload instanceof Uint8Array)) return refuse();
      reserveContextBytes(computeDeepBytes(v as ExprValue) - SLOT, walk);
      return v as ExprValue;
    case "list":
    case "tuple": {
      if (!Array.isArray(payload)) return refuse();
      const source = payload as readonly unknown[];
      const length = source.length;
      checkContextCapacity(length, walk);
      // Refuse the minimum payload size before allocating traversal scratch or reading members.
      walk.budget.checkSize(walk.bytes + SLOT * length);
      walk.budget.checkAvailable(walk.bytes + SLOT * length);
      const items: ExprValue[] = [];
      for (let i = 0; i < length; i++) {
        // Array.map skips holes. Missing own members must fail as undefined, even if a prototype
        // supplies that index, before a value constructor or Budget.admit sees the collection.
        const item = Object.hasOwn(source, i) ? contextData(source, i, path) : undefined;
        items.push(admitContextValue(item, `${path}[${i}]`, depth + 1, walk));
      }
      return { kind, items };
    }
    case "dict": {
      if (!Array.isArray(payload)) return refuse();
      checkContextCapacity(payload.length, walk);
      return admitContextEntries(payload as readonly unknown[], path, depth + 1, walk);
    }
    default:
      return refuse();
  }
}

function keyLabel(key: ExprValue): string {
  return key.kind === "str" ? `'${key.value}'` : key.kind === "int" ? `${key.value}` : `<${key.kind}>`;
}

/** Primitive keys use an index; compound Python equality is explicitly bounded by work charges. */
function admitContextEntries(entries: Iterable<unknown>, path: string, depth: number, walk: ContextWalk): DictValue {
  const indexed = new Set<string>();
  const otherKeys: ExprValue[] = [];
  const output: (readonly [ExprValue, ExprValue])[] = [];
  for (const entry of Array.isArray(entries) ? contextArrayEntries(entries) : entries) {
    if (!Array.isArray(entry) || entry.length !== 2) {
      throw new UIError(`${path} entries must be [key, value] pairs, got ${describeHostValue(entry)}`, "value");
    }
    checkContextCapacity(1, walk);
    reserveContextBytes(16, walk);
    const rawKey = contextData(entry, 0, path);
    const key = admitContextValue(typeof rawKey === "string" ? strVal(rawKey) : rawKey, `${path} key`, depth, walk);
    if (!isHashable(key)) throw new UIError(`${path} key ${keyLabel(key)} is unhashable (kind '${key.kind}')`, "value");
    let duplicate = false;
    const token = key.kind === "str" ? `s:${key.value}` : key.kind === "none" ? "none"
      : key.kind === "int" ? `n:${key.value}` : key.kind === "bool" ? `n:${key.value ? 1 : 0}`
      : key.kind === "float" && !Number.isNaN(key.value)
        ? (Number.isFinite(key.value) && Number.isInteger(key.value) ? `n:${BigInt(key.value)}` : `f:${key.value}`)
        : undefined;
    if (token !== undefined) {
      duplicate = indexed.has(token);
      indexed.add(token);
    } else if (key.kind !== "float") { // NaN is never equal to another key, including itself.
      for (const previous of otherKeys) {
        walk.budget.steps(computeNodes(previous) + computeNodes(key));
        walk.budget.scan(computeDeepBytes(previous) + computeDeepBytes(key));
        if (keysEqual(previous, key)) { duplicate = true; break; }
      }
      otherKeys.push(key);
    }
    if (duplicate) throw new UIError(`${path} has a duplicate key ${keyLabel(key)}`, "value");
    output.push([key, admitContextValue(contextData(entry, 1, path), `${path}[${keyLabel(key)}]`, depth, walk)]);
  }
  return { kind: "dict", entries: output };
}

/** Iterate data arrays by own index, never by a host-supplied Symbol.iterator. */
function* contextArrayEntries(entries: readonly unknown[]): Iterable<unknown> {
  for (let i = 0; i < entries.length; i++) {
    yield Object.hasOwn(entries, i) ? contextData(entries, i, "session.context") : undefined;
  }
}

/** Yields own enumerable values only after the caller has bounded the mapping's key count. */
function* contextObjectEntries(raw: Record<string, unknown>): Iterable<readonly [string, unknown]> {
  for (const key in raw) {
    if (Object.hasOwn(raw, key)) yield [key, contextData(raw, key, "session.context")];
  }
}

function admitContext(raw: unknown, budget: Budget): DictValue {
  const limits = budget.limits;
  const walk: ContextWalk = { values: 0, maxValues: limits.maxSteps, maxDepth: limits.maxDepth, bytes: 0, budget };
  // Root dictionary reservation precedes even requesting the mapping's values.
  reserveContextBytes(SLOT, walk);
  const path = "session.context";
  if (raw instanceof PyDict) {
    checkContextCapacity(pyDictLength.call(raw) as number, walk);
    return admitContextEntries(pyDictEntries.call(raw) as readonly unknown[], path, 1, walk);
  }
  if (raw instanceof Map) {
    checkContextCapacity(mapSize.call(raw) as number, walk);
    return admitContextEntries(mapEntries.call(raw), path, 1, walk);
  }
  if (typeof raw === "object" && raw !== null && !Array.isArray(raw)) {
    if (Object.getOwnPropertyDescriptor(raw, "kind")?.value === "dict") {
      walk.bytes = 0; // tagged dictionaries count their own root
      return admitContextValue(raw, path, 0, walk) as DictValue;
    }
    if (Object.getPrototypeOf(raw) === Object.prototype || Object.getPrototypeOf(raw) === null) {
      // Count without Object.entries/keys: neither values nor a full key/pair array are copied
      // before the admission check. Inherited and non-enumerable properties are not context data.
      let count = 0;
      for (const key in raw) {
        if (Object.hasOwn(raw, key)) checkContextCapacity(++count, walk);
      }
      return admitContextEntries(contextObjectEntries(raw as Record<string, unknown>), path, 1, walk);
    }
  }
  throw new UIError(`invalid context binding: expected a dict or mapping, got ${describeHostValue(raw)}`, "value");
}

/** What a live handle holds; dropped as a whole on disposal so the handle retains none of it. */
interface LiveState {
  readonly ast: ValidatedAst;
  readonly record: RecordResolver;
  readonly context: SessionBindings["context"];
  readonly parent: RecordExpressionOptions["parent"];
}

/**
 * Creates a memoized reactive expression handle backed by Elang `computed`.
 * Lazily evaluates record and parent attributes, tracks branch-dependent subscriptions,
 * enforces fresh budgets per evaluation, and supports explicit and owner-attached disposal.
 */
export function createRecordExpression(
  ast: ValidatedAst,
  options: RecordExpressionOptions,
): ExpressionHandle {
  if (!isValidatedAst(ast)) {
    throw new UIError("createRecordExpression requires a ValidatedAst returned by decodeAst", "value");
  }
  if (!options || typeof options !== "object") {
    throw new UIError("createRecordExpression requires options", "value");
  }

  // Validate session bindings
  if (!options.session || typeof options.session !== "object") {
    throw new UIError("createRecordExpression requires session bindings", "value");
  }
  for (const k of Object.keys(options.session)) {
    if (!ALLOWED_SESSION_KEYS.has(k)) {
      throw new UIError(`unknown session binding: '${k}'`, "value");
    }
  }

  const uidBig = checkI64(options.session.uid, "session.uid");
  const admittedUid = intVal(uidBig);

  if (!Array.isArray(options.session.allowed_company_ids)) {
    throw new UIError("session.allowed_company_ids must be an array of i64 integers", "value");
  }
  const companyIds = options.session.allowed_company_ids;
  const maxCompanyIds = Math.floor((EVAL_LIMITS_CEILING.maxValueBytes - SLOT) / SLOT);
  if (companyIds.length > maxCompanyIds) {
    throw new UIError(`session.allowed_company_ids holds more than ${maxCompanyIds} ids`, "value");
  }
  const normalizedCompanies: ExprValue[] = [];
  for (let index = 0; index < companyIds.length; index++) {
    const descriptor = Object.getOwnPropertyDescriptor(companyIds, index);
    if (descriptor && !("value" in descriptor)) {
      throw new UIError(`session.allowed_company_ids[${index}] must be an integer data property`, "value");
    }
    normalizedCompanies.push(intVal(checkI64(descriptor?.value, `session.allowed_company_ids[${index}]`)));
  }
  const admittedAllowedCompanies = listVal(normalizedCompanies);

  let admittedCurrentCompany: ExprValue = NONE;
  if (
    options.session.current_company_id !== undefined &&
    options.session.current_company_id !== null &&
    (options.session.current_company_id as unknown) !== NONE
  ) {
    admittedCurrentCompany = intVal(
      checkI64(options.session.current_company_id, "session.current_company_id"),
    );
  }

  if (options.session.context === undefined || options.session.context === null) {
    throw new UIError("session.context must be supplied", "value");
  }

  // Validate id without invoking data accessors or coercing a wrong bool payload to False.
  let admittedId: ExprValue;
  const rawId: unknown = options.id;
  if (rawId === false) {
    admittedId = FALSE;
  } else if (typeof rawId === "object" && rawId !== null && Object.getOwnPropertyDescriptor(rawId, "kind")?.value === "bool") {
    const payload = Object.getOwnPropertyDescriptor(rawId, "value");
    if (!payload || !("value" in payload) || typeof payload.value !== "boolean") {
      throw new UIError("unsaved record id must be False, got invalid bool payload", "value");
    }
    if (payload.value) throw new UIError("unsaved record id must be False, not True", "value");
    admittedId = FALSE;
  } else if (rawId === true) {
    throw new UIError("unsaved record id must be False, not True", "value");
  } else {
    admittedId = intVal(checkI64(rawId, "id"));
  }

  // Normalize record resolver and check for reserved collisions.
  const normRecord = toResolver(options.record);
  assertNoReservedFields(normRecord);

  // Resolve reserved names and record fields before evaluator-owned builtins.
  // createContext supplies the fallback and keeps its provenance evaluation-local.
  const limits = new Budget(options.limits).limits;

  let state: LiveState | null = {
    ast,
    record: normRecord,
    context: options.session.context,
    parent: options.parent,
  };

  // State for computed lifecycle and disposal
  const DISPOSED_SENTINEL = Symbol("erp.disposed_sentinel");
  const lifecycle = signal(0);
  let disposed = false;
  let inEvaluation = false;

  function live(): LiveState {
    if (disposed || state === null) {
      throw new DisposedExpression("expression handle was disposed during evaluation");
    }
    return state;
  }

  // Validate once per evaluation (N2), then revalidate when a later evaluation observes a replacement.
  let contextBudget: Budget | undefined;
  let contextValue: DictValue | undefined;
  function resolveContext(s: LiveState, budget: Budget): DictValue {
    if (contextBudget !== budget || contextValue === undefined) {
      const raw = typeof s.context === "function" ? (s.context as () => unknown)() : s.context;
      contextValue = admitContext(raw, budget);
      contextBudget = budget;
    }
    return contextValue;
  }

  function resolveParent(s: LiveState): RecordResolver | null {
    if (s.parent === undefined || s.parent === null) return null;
    const p = typeof s.parent === "function"
      ? (s.parent as () => ParentResolver | null | undefined)()
      : s.parent;
    if (p === undefined || p === null) return null;
    return toResolver(p);
  }

  const parentExprObject: ExprObject = {
    typeName: () => "Record",
    getAttr(attr: string, _budget: Budget): ExprValue {
      const p = resolveParent(live());
      if (!p) throw attributeError("Record", attr);
      const val = p.getField(attr);
      if (val === undefined) {
        throw attributeError("Record", attr);
      }
      // The evaluator validates, charges and snapshots this attribute result exactly once.
      return val;
    },
  };

  // Built once per handle (review F2); `evalAst` starts a fresh Budget from `limits` on every run.
  function resolveName(name: string, budget: Budget): ExprValue | undefined {
      const s = live();
      switch (name) {
        case "id":
          return admittedId;
        case "uid":
          return admittedUid;
        case "allowed_company_ids":
          return admittedAllowedCompanies;
        case "current_company_id":
          return admittedCurrentCompany;
        case "context":
          return resolveContext(s, budget);
        case "parent":
          if (!resolveParent(s)) throw nameError("parent");
          return objectVal(parentExprObject);
      }
      const field = s.record.getField(name);
      if (field !== undefined) return field;
      if (s.record.hasField?.(name)) {
        throw new UIError(`record field '${name}' is declared but has no value`, "value");
      }
      return undefined;
  }
  const evalCtx = createContext({}, limits, undefined, resolveName);

  const comp = computed(() => {
    lifecycle();
    if (disposed || state === null) return DISPOSED_SENTINEL;
    inEvaluation = true;
    try {
      const result = evalAst(state.ast, evalCtx);
      if (disposed) {
        throw new DisposedExpression("expression handle was disposed during evaluation");
      }
      return result;
    } finally {
      inEvaluation = false;
      contextBudget = undefined;
      contextValue = undefined;
    }
  });

  /**
   * Re-runs the computation once, untracked, after `disposed` is set: it reads only `lifecycle`, so the
   * core drops every field, parent and context subscription. Nothing is caught: after the lifecycle
   * write the computation is dirty and returns the sentinel without evaluating, so any error here is a
   * real failure and propagates. Then the handle drops its AST, record and bindings.
   */
  function detach(): void {
    let v: unknown;
    try {
      v = untrack(comp);
    } finally {
      // Released even when the re-read throws (review N3).
      state = null;
    }
    if (v !== DISPOSED_SENTINEL) {
      throw new UIError("expression handle disposal did not detach its computation (runtime invariant)", "usage");
    }
  }

  function dispose(): void {
    if (disposed) return;
    disposed = true;
    lifecycle.set(untrack(lifecycle) + 1);
    // Disposed from inside its own evaluation (a resolver called dispose): re-reading the computation now
    // would re-enter it, so `read` detaches once the evaluation has unwound.
    if (inEvaluation) return;
    detach();
  }

  const handle: ExpressionHandle = {
    get read(): ExprValue {
      if (disposed) {
        throw new DisposedExpression("expression handle has been disposed");
      }
      let val: unknown;
      try {
        val = comp();
      } catch (e) {
        if (!disposed) throw e;
        try {
          detach();
        } catch (detachError) {
          // Keep both failures: the evaluation error and the detach error (review N3).
          throw new UIError("expression handle could not detach after a failed evaluation", "usage", {
            cause: new AggregateError([e, detachError], "evaluation error, then detach error"),
          });
        }
        if (e instanceof DisposedExpression) throw e;
        // The evaluation failed after the disposal for another reason: report the disposal, keep the
        // failure as its cause.
        throw new DisposedExpression("expression handle was disposed during evaluation", { cause: e });
      }
      if (disposed) {
        detach();
        throw new DisposedExpression("expression handle was disposed during evaluation");
      }
      return val as ExprValue;
    },
    dispose,
    get isDisposed(): boolean {
      return disposed;
    },
  };

  const owner = options.owner !== undefined ? options.owner : getOwner();
  if (owner !== null && !owner.disposed) {
    owner.onCleanup(() => {
      handle.dispose();
    });
  }

  return handle;
}

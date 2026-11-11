/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Component, Registry, RegistryHub, patch, PatchSet, useService, onWillDestroy, onWillStart, onWillUpdateProps, signal, flushSync } from "../../../src/index.js";
import { createTestScope, withTestScope, MockRpc } from "../src/index.js";
import type { TestOnlyResponse } from "../../../src/generated/TestOnlyResponse.js";
import type { Company } from "../../../../addons/web/static/src/generated/human-ui.js";

declare module "../../../src/runtime/registry.js" { interface Registries { harness: string; } }
const isString = (x: unknown): x is string => typeof x === "string";
function fails(fn: () => unknown, code: string, message: string): void {
  let error: unknown;
  try { fn(); } catch (cause) { error = cause; }
  expect(error).toBeInstanceOf(Error);
  expect(error).toMatchObject({ code, message });
}

/// (partial: managed object, static and addon-owned rollback)
test("managed patches automatically restore direct, static and PatchSet layers", async () => {
  const obj = { value() { return 1; } };
  class Static { static value() { return 2; } }
  await withTestScope(async scope => {
    scope.patch(obj, sup => ({ value() { return sup.value.call(this) + 1; } }));
    scope.patchStatic(Static, sup => ({ value() { return sup.value.call(this) + 1; } }));
    scope.manage(() => { new PatchSet("test").patch(obj, sup => ({ value() { return sup.value.call(this) + 2; } })); });
    expect(obj.value()).toBe(4); expect(Static.value()).toBe(3);
  });
  expect(obj.value()).toBe(1); expect(Static.value()).toBe(2);
});

/// (partial: unmanaged leak fails and subsequent scope observes baseline)
test("a direct leaked patch fails its test boundary and still restores the next scope", async () => {
  const obj = { value() { return 1; } };
  await expect(withTestScope(async () => { patch(obj, () => ({ value() { return 9; } })); }))
    .rejects.toMatchObject({ name: "HarnessError", code: "leaked-patch", message: 'Unmanaged patches remain at test teardown: value' });
  expect(obj.value()).toBe(1);
  await withTestScope(async () => { expect(obj.value()).toBe(1); });
});

/// (partial: manual removal and pre-existing layer preservation)
test("manual removal is not a leak, and restores a pre-existing patch", async () => {
  const obj = { value() { return 1; } };
  const outside = patch(obj, sup => ({ value() { return sup.value.call(this) + 1; } }));
  try {
    await withTestScope(async () => {
      const undo = patch(obj, sup => ({ value() { return sup.value.call(this) + 3; } }));
      expect(obj.value()).toBe(5); undo(); undo();
    });
    expect(obj.value()).toBe(2);
  } finally { outside(); }
});

/// (partial: entry values, sequence, equal-sequence ordering and category rollback)
test("registries restore exact values, sequence, tie order and category definitions", async () => {
  const registry = new Registry("shared", isString).add("a", "A", { sequence: 3 }).add("b", "B", { sequence: 3 });
  const hub = new RegistryHub();
  await withTestScope(async () => {
    registry.remove("a"); registry.add("a", "changed", { sequence: 9 });
    registry.add("b", "changed", { force: true });
    hub.define("harness", isString).add("x", "X");
  });
  expect(registry.getEntries()).toEqual([["a", "A"], ["b", "B"]]);
  registry.add("c", "C", { sequence: 4 });
  expect(registry.getAll()).toEqual(["A", "B", "C"]);
  fails(() => hub.category("harness"), "registry-missing", 'Registry category "harness" is not defined');
});

test("overlapping scopes fail loudly without disturbing the active scope", async () => {
  const first = createTestScope();
  try { fails(() => createTestScope(), "scope-active", "A component test scope is already active"); }
  finally { await first.dispose(); }
});

/// (partial: body failure combined with leak failure)
test("callback and cleanup failure remain observable together", async () => {
  const cause = new Error("body failed");
  const obj = { value() { return 1; } };
  let error: unknown;
  try { await withTestScope(async () => { patch(obj, () => ({ value() { return 2; } })); throw cause; }); }
  catch (e) { error = e; }
  expect(error).toBeInstanceOf(AggregateError);
  expect((error as AggregateError).errors[0]).toBe(cause);
  expect((error as AggregateError).errors[1]).toMatchObject({ code: "leaked-patch" });
  expect(obj.value()).toBe(1);
});

/// (partial: real mount and lifecycle teardown)
/// (partial: canonical service mock identity)
test("mount uses real services and destroys its attached root", async () => {
  class Labels { static start() { return new Labels(); } text() { return "real"; } }
  let destroyed = false;
  class Root extends Component { static override template = "HarnessRoot"; override setup() { const service = useService(Labels); onWillDestroy(() => { destroyed = true; }); this.text = service.text(); } text = ""; }
  let root: HTMLElement;
  await withTestScope(async scope => {
    scope.mockService(Labels, { text() { return "mock"; } });
    const mounted = await scope.mount(Root, { templates: [{ name: "HarnessRoot", render: self => document.createTextNode((self as Root).text) }] });
    root = mounted.root; expect(root.textContent).toBe("mock"); expect(root.isConnected).toBe(true);
  });
  expect(destroyed).toBe(true); expect(root!.isConnected).toBe(false);
});

/// (partial: failed mount cleanup)
test("failed mounts clean their target and preserve the original cause", async () => {
  const cause = new Error("setup failed");
  class Bad extends Component { static override template = "Bad"; override setup() { throw cause; } }
  const before = document.body.childElementCount;
  await expect(withTestScope(async scope => { await scope.mount(Bad, { templates: [{ name: "Bad", render: () => null }] }); })).rejects.toBe(cause);
  expect(document.body.childElementCount).toBe(before);
});

type Contract = { company: { request: { id: number }; response: Company }; schema: { request: { key: string }; response: TestOnlyResponse } };
/// (partial: runtime dispatch, recording and errors; type checks are in types.ts)
test("RPC mock preserves generated DTO results, records calls and refuses missing handlers", async () => {
  const rpc = new MockRpc<Contract>();
  rpc.on("company", ({ id }) => ({ id, name: "A", parent_id: null }));
  const company = await rpc.call("company", { id: 7 });
  expect(company.id).toBe(7); expect(rpc.calls).toEqual([{ operation: "company", request: { id: 7 } }]);
  await expect(rpc.call("schema", { key: "x" })).rejects.toMatchObject({ code: "rpc-unhandled", message: 'No mock RPC handler for "schema"' });
  const cause = new Error("RPC failed"); rpc.on("company", () => { throw cause; });
  await expect(rpc.call("company", { id: 8 })).rejects.toBe(cause);
});

test("closed scopes refuse operations and async manage never enters its callback", async () => {
  const scope = createTestScope(); let entered = false;
  fails(() => (scope.manage.bind(scope) as (fn: () => unknown) => unknown)(async () => { entered = true; }), "async-manage", "Test scope manage requires a synchronous callback");
  expect(entered).toBe(false);
  await scope.dispose();
  fails(() => scope.manage(() => 1), "scope-closed", "Component test scope is closed");
  await expect(scope.mount(Component, { templates: [] })).rejects.toMatchObject({ code: "scope-closed", message: "Component test scope is closed" });
});

test("service startup failure is preserved and creates no orphan root", async () => {
  const cause = new Error("start failed");
  class Broken { static start(): Broken { throw cause; } }
  const before = document.body.childElementCount;
  await withTestScope(async scope => {
    scope.services.add(Broken);
    await expect(scope.mount(Component, { templates: [] })).rejects.toMatchObject({ code: "service-start", message: "Service Broken failed to start", cause });
  });
  expect(document.body.childElementCount).toBe(before);
});

test("disposing during pending service startup prevents a late mount", async () => {
  let finish!: () => void;
  let signalStarted!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  class Slow { static start() { return new Promise<Slow>(resolve => { finish = () => resolve(new Slow()); signalStarted(); }); } }
  const scope = createTestScope(); scope.services.add(Slow);
  const before = document.body.childElementCount;
  const mounting = scope.mount(Component, { templates: [] });
  await started;
  const disposing = scope.dispose(); finish();
  await disposing;
  await expect(mounting).rejects.toMatchObject({ code: "scope-closed", message: "Component test scope is closed" });
  expect(document.body.childElementCount).toBe(before);
});

test("missing service dependencies retain their precise failure", async () => {
  class Dependency { static start() { return new Dependency(); } }
  class Consumer { static dependencies = [Dependency]; static start() { return new Consumer(); } }
  await withTestScope(async scope => {
    scope.services.add(Consumer);
    await expect(scope.mount(Component, { templates: [] })).rejects.toMatchObject({ code: "service-missing-dependency", message: "Service Consumer depends on unregistered service Dependency" });
  });
});

/// (partial: cleanup failure preserves the body failure and still restores other resources)
test("rollback failure attempts other patches and preserves both failures", async () => {
  const original = new Error("body failure");
  let blocked = false;
  const target = new Proxy({ value(): number { return 1; } }, {
    defineProperty(object, key, descriptor) { if (blocked) throw new Error("fixture rejects restore"); return Reflect.defineProperty(object, key, descriptor); },
  });
  const other = { value() { return 3; } };
  const registry = new Registry("cleanup", isString).add("before", "B");
  let undo!: () => void;
  let error: unknown;
  try {
    await withTestScope(async scope => {
      undo = scope.patch(target, () => ({ value() { return 2; } }));
      scope.patch(other, () => ({ value() { return 4; } }));
      registry.add("during", "D");
      blocked = true; throw original;
    });
  } catch (cause) { error = cause; }
  finally { blocked = false; undo(); }
  expect(error).toBeInstanceOf(AggregateError);
  const errors = (error as AggregateError).errors;
  expect(errors[0]).toBe(original);
  expect(errors[1]).toMatchObject({ name: "AggregateError", message: "Component test mutation rollback failed" });
  expect(other.value()).toBe(3); expect(target.value()).toBe(1);
  expect(registry.getEntries()).toEqual([["before", "B"]]);
  await withTestScope(async () => { expect(other.value()).toBe(3); });
});

/// (partial: restored object identity and insertion sequence)
test("restored registry entries retain payload identity and original tie order", async () => {
  const first = { label: "first" }, second = { label: "second" };
  const registry = new Registry("objects", (x): x is { label: string } => typeof x === "object" && x !== null && "label" in x);
  registry.add("first", first, { sequence: 2 }).add("second", second, { sequence: 2 });
  await withTestScope(async () => { registry.remove("first"); registry.add("first", { label: "replacement" }, { sequence: 9 }); registry.add("second", { label: "forced" }, { force: true }); });
  expect(registry.get("first")).toBe(first); expect(registry.get("second")).toBe(second);
  registry.add("third", { label: "third" }, { sequence: 2 });
  expect(registry.getEntries().map(([key]) => key)).toEqual(["first", "second", "third"]);
});

/// (partial: disposal while runtime mount is pending)
test("disposing a pending real mount removes the runtime application", async () => {
  let finish!: () => void;
  let signalStarted!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  class Pending extends Component {
    static override template = "PendingScope";
    override setup() { onWillStart(() => new Promise<void>(resolve => { finish = resolve; signalStarted(); })); }
  }
  const scope = createTestScope();
  const before = document.body.childElementCount;
  const mounting = scope.mount(Pending, { templates: [{ name: "PendingScope", render: () => null }] });
  const outcome = mounting.then(value => ({ value }), error => ({ error }));
  await started;
  const disposing = scope.dispose(); finish();
  await disposing;
  const result = await outcome;
  expect(result).toMatchObject({ error: { name: "UIError", code: "usage", message: "unmount: the app was unmounted before it finished mounting" } });
  expect(document.body.childElementCount).toBe(before);
});

/// (partial: canonical startup restoration and per-scope instances)
test("mock services restore the original startup and another scope has a fresh instance", async () => {
  class Service { static start() { return new Service(); } label() { return "real"; } }
  const originalStart = Service.start;
  let first: Service;
  await withTestScope(async scope => {
    scope.mockService(Service, { label() { return "mock"; } });
    await scope.services.start(scope.env); first = scope.services.get(Service);
    expect(first.label()).toBe("mock");
  });
  expect(Service.start).toBe(originalStart);
  await withTestScope(async scope => {
    scope.services.add(Service); await scope.services.start(scope.env);
    expect(scope.services.get(Service)).not.toBe(first!);
    expect(scope.services.get(Service).label()).toBe("real");
  });
});

/// (partial: tracked service startup retains mutation ownership through disposal)
test("disposal waits for tracked service startup and captures its late mutations", async () => {
  let release!: () => void;
  let signalStarted!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  const target = { value() { return 1; } };
  const registry = new Registry("late-service", isString).add("base", "B");
  let undo: (() => void) | undefined;
  class Slow {
    static async start() {
      await new Promise<void>(resolve => { release = resolve; signalStarted(); });
      undo = patch(target, () => ({ value() { return 2; } }));
      registry.add("late", "L");
      return new Slow();
    }
  }
  const scope = createTestScope(); scope.services.add(Slow);
  const mounting = scope.mount(Component, { templates: [] }).then(value => ({ value }), error => ({ error }));
  await started;
  const cleanup = scope.dispose();
  expect(scope.dispose()).toBe(cleanup);
  const disposal = cleanup.then(() => null, error => error);
  let overlapError: unknown;
  let extra: ReturnType<typeof createTestScope> | undefined;
  try { extra = createTestScope(); } catch (error) { overlapError = error; }
  if (extra !== undefined) await extra.dispose();
  release();
  const disposalError = await disposal; await mounting;
  try {
    expect(overlapError).toMatchObject({ code: "scope-active", message: "A component test scope is already active" });
    expect(disposalError).toMatchObject({ code: "leaked-patch", message: "Unmanaged patches remain at test teardown: value" });
    expect(target.value()).toBe(1); expect(registry.getEntries()).toEqual([["base", "B"]]);
    await withTestScope(async () => { expect(target.value()).toBe(1); });
  } finally { undo?.(); registry.remove("late"); }
});

/// (partial: underlying onWillStart work remains tracked after unmount rejects mount)
test("disposal drains underlying onWillStart work before releasing mutation ownership", async () => {
  let release!: () => void;
  let signalStarted!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  const target = { value() { return 1; } };
  let undo: (() => void) | undefined;
  class Slow extends Component {
    static override template = "LateLifecycle";
    override setup() { onWillStart(async () => { await new Promise<void>(resolve => { release = resolve; signalStarted(); }); undo = patch(target, () => ({ value() { return 8; } })); }); }
  }
  const scope = createTestScope();
  const mounting = scope.mount(Slow, { templates: [{ name: "LateLifecycle", render: () => null }] }).then(value => ({ value }), error => ({ error }));
  await started;
  const cleanup = scope.dispose();
  expect(scope.dispose()).toBe(cleanup);
  const disposal = cleanup.then(() => null, error => error);
  let overlapError: unknown;
  let extra: ReturnType<typeof createTestScope> | undefined;
  try { extra = createTestScope(); } catch (error) { overlapError = error; }
  if (extra !== undefined) await extra.dispose();
  release();
  const error = await disposal; await mounting;
  try {
    expect(overlapError).toMatchObject({ code: "scope-active", message: "A component test scope is already active" });
    expect(error).toMatchObject({ code: "leaked-patch", message: "Unmanaged patches remain at test teardown: value" });
    expect(target.value()).toBe(1);
  } finally { undo?.(); }
});

/// (partial: unexpected async manage results are observed and drained)
test("unsafe Promise-returning manage preserves rejection without an unhandled rejection", async () => {
  const scope = createTestScope(); const cause = new Error("async manage failed");
  const unsafeManage = scope.manage.bind(scope) as (fn: () => unknown) => unknown;
  fails(() => unsafeManage(() => Promise.reject(cause)), "async-manage", "Test scope manage requires a synchronous callback");
  const error = await scope.dispose().then(() => null, failure => failure);
  expect(error).toBe(cause);
});

/// (partial: PatchSet ownership metadata and capacity survive automatic cleanup)
test("a retained PatchSet is emptied by scope teardown and reusable in another scope", async () => {
  const set = new PatchSet("retained"); const target = { value() { return 1; } };
  await withTestScope(async scope => { scope.manage(() => set.patch(target, () => ({ value() { return 2; } }))); });
  expect(target.value()).toBe(1); expect(set.getPatches()).toEqual([]);
  await withTestScope(async scope => { scope.manage(() => set.patch(target, () => ({ value() { return 3; } }))); });
  expect(target.value()).toBe(1); expect(set.getPatches()).toEqual([]);
});

/// (partial: unsafe manage continuations retain ownership until settled)
test("unsafe thenable manage drains a late patch before permitting another scope", async () => {
  let release!: () => void;
  const paused = new Promise<void>(resolve => { release = resolve; });
  const scope = createTestScope(); const target = { value() { return 1; } };
  const unsafeManage = scope.manage.bind(scope) as (fn: () => unknown) => unknown;
  fails(() => unsafeManage(() => paused.then(() => { patch(target, () => ({ value() { return 9; } })); })), "async-manage", "Test scope manage requires a synchronous callback");
  const disposal = scope.dispose().then(() => null, error => error);
  fails(() => createTestScope(), "scope-active", "A component test scope is already active");
  release();
  expect(await disposal).toMatchObject({ code: "leaked-patch", message: "Unmanaged patches remain at test teardown: value" });
  expect(target.value()).toBe(1);
});

/// (partial: PatchSet failed cleanup preserves retryable provenance)
test("failed automatic PatchSet removal remains visible and retryable", async () => {
  let blocked = false;
  const target = new Proxy({ value(): number { return 1; } }, {
    defineProperty(object, key, descriptor) { if (blocked) throw new Error("set restore blocked"); return Reflect.defineProperty(object, key, descriptor); },
  });
  const set = new PatchSet("retry");
  const scope = createTestScope();
  scope.manage(() => { set.patch(target, () => ({ value() { return 2; } })); });
  blocked = true;
  const error = await scope.dispose().then(() => null, cause => cause);
  try {
    expect(error).toMatchObject({ name: "AggregateError", message: "Component test mutation rollback failed" });
    expect(set.getPatches()).toEqual([{ addon: "retry", target, member: "value" }]);
  } finally { blocked = false; set.unpatchAll(); }
  expect(set.getPatches()).toEqual([]); expect(target.value()).toBe(1);
});

/// (partial: pending prop-update lifecycle work stays owned during disposal)
test("disposal drains pending onWillUpdateProps work", async () => {
  let release!: () => void, signalStarted!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  const target = { value() { return 1; } }; let undo: (() => void) | undefined;
  const value = signal(0);
  class Updating extends Component<{ value: number }> {
    static override template = "UpdatingScope";
    override setup() { onWillUpdateProps(async () => { await new Promise<void>(resolve => { release = resolve; signalStarted(); }); undo = patch(target, () => ({ value() { return 2; } })); }); }
  }
  const scope = createTestScope();
  await scope.mount(Updating, { props: { get value() { return value(); } }, templates: [{ name: "UpdatingScope", render: () => null }] });
  value.set(1); flushSync(); await started;
  const disposal = scope.dispose().then(() => null, error => error);
  let overlap: unknown, extra: ReturnType<typeof createTestScope> | undefined;
  try { extra = createTestScope(); } catch (error) { overlap = error; }
  if (extra !== undefined) await extra.dispose();
  release(); const error = await disposal;
  // Drain the known callback continuation even on the broken implementation.
  await Promise.resolve(); await Promise.resolve();
  try {
    expect(overlap).toMatchObject({ code: "scope-active", message: "A component test scope is already active" });
    expect(error).toMatchObject({ code: "leaked-patch", message: "Unmanaged patches remain at test teardown: value" });
    expect(target.value()).toBe(1);
  } finally { undo?.(); }
});

test("manage accepts synchronous objects with a non-callable then member", async () => {
  await withTestScope(async scope => { const value = { then: "label" }; expect(scope.manage(() => value)).toBe(value); });
});

test("disposal reentry from teardown callbacks refuses instead of awaiting itself", async () => {
  const scope = createTestScope();
  let reentered: Promise<unknown> | undefined;
  class Reentrant extends Component {
    static override template = "ReentrantDispose";
    override setup() { onWillDestroy(() => { reentered = scope.dispose().then(() => null, error => error); }); }
  }
  await scope.mount(Reentrant, { templates: [{ name: "ReentrantDispose", render: () => null }] });
  const cleanup = scope.dispose();
  await cleanup;
  expect(await reentered).toMatchObject({ code: "scope-closed", message: "Cannot dispose a component test scope from its teardown callbacks" });
  expect(scope.dispose()).toBe(cleanup);
});

/// (partial: asynchronous teardown cannot await its own scope cleanup)
test("async teardown disposal reentry rejects, restores mutations and releases the next scope", async () => {
  const target = { value() { return 1; } };
  const registry = new Registry("async-teardown", isString).add("base", "B");
  const scope = createTestScope();
  const reported: unknown[] = [];
  class AsyncTeardown extends Component {
    static override template = "AsyncTeardown";
    override setup() {
      onWillDestroy((async () => {
        await Promise.resolve();
        patch(target, () => ({ value() { return 9; } }));
        registry.add("late", "L");
        await scope.dispose();
      }) as unknown as () => void);
    }
  }
  await scope.mount(AsyncTeardown, { templates: [{ name: "AsyncTeardown", render: () => null }], onError: error => reported.push(error) });
  const error = await scope.dispose().then(() => null, cause => cause);
  expect(error).toBeInstanceOf(AggregateError);
  expect((error as AggregateError).errors[0]).toMatchObject({ code: "scope-closed", message: "Cannot dispose a component test scope while teardown callbacks are pending" });
  expect((error as AggregateError).errors[1]).toMatchObject({ code: "leaked-patch", message: "Unmanaged patches remain at test teardown: value" });
  expect(reported[0]).toBe((error as AggregateError).errors[0]);
  expect(target.value()).toBe(1); expect(registry.getEntries()).toEqual([["base", "B"]]);
  await withTestScope(async () => { expect(target.value()).toBe(1); });
});

test("extra disposal calls refuse while tracked async teardown is pending", async () => {
  const scope = createTestScope();
  let signalStarted!: () => void, release!: () => void;
  const started = new Promise<void>(resolve => { signalStarted = resolve; });
  const gate = new Promise<void>(resolve => { release = resolve; });
  class AsyncCleanup extends Component {
    static override template = "AsyncCleanup";
    override setup() { onWillDestroy((async () => { signalStarted(); await gate; }) as unknown as () => void); }
  }
  await scope.mount(AsyncCleanup, { templates: [{ name: "AsyncCleanup", render: () => null }] });
  const cleanup = scope.dispose(); await started;
  const repeat = scope.dispose().then(() => null, error => error);
  release(); await cleanup;
  expect(await repeat).toMatchObject({ code: "scope-closed", message: "Cannot dispose a component test scope while teardown callbacks are pending" });
  expect(scope.dispose()).toBe(cleanup);
});

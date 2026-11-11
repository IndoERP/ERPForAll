/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  UIError, Component, Registry, RegistryHub, ExtensionError, ServiceContainer, createEnv,
  useEnv, useSubEnv, useService, mount, unmount, flushSync, patch, patchStatic, registerAddons,
  type Env, type ServiceResolver, type ServiceClass,
} from "../src/index.js";
import { RegistryHub as PublicRegistryHub } from "@erpforall/elang";
import { settle, target, tpl } from "./harness.js";

declare module "@erpforall/elang" {
  interface Registries { labels: string; badges: { title: string }; [key: `label_${number}`]: string; }
  interface Services { labels: Labels; }
}
const isString = (v: unknown): v is string => typeof v === "string";
function fails(fn: () => unknown, code: string, message: string): void {
  let error: unknown;
  try { fn(); } catch (e) { error = e; }
  expect(error).toBeInstanceOf(ExtensionError);
  expect(error).toBeInstanceOf(UIError);
  expect(error).toMatchObject({ code, message });
}
function throwsCause(fn: () => unknown, cause: Error): void {
  const message = cause.message;
  let error: unknown;
  try { fn(); } catch (e) { error = e; }
  expect(error).toBe(cause);
  expect(error).toBeInstanceOf(Error);
  expect(error).toMatchObject({ name: "Error", message });
}
async function rejects(fn: () => Promise<unknown>, code: string, message: string): Promise<void> {
  let error: unknown;
  try { await fn(); } catch (e) { error = e; }
  expect(error).toBeInstanceOf(ExtensionError);
  expect(error).toBeInstanceOf(UIError);
  expect(error).toMatchObject({ code, message });
}

/// (partial: typed categories declared with required schemas, no implicit unvalidated categories)
test("categories are typed, cached and require a schema", () => {
  const hub = new RegistryHub();
  const labels = hub.define("labels", isString);
  expect(hub.category("labels")).toBe(labels);
  expect(hub.category("labels")).toBe(labels);
  fails(() => new RegistryHub().category("labels"), "registry-missing", 'Registry category "labels" is not defined');
  fails(() => hub.define("labels", isString), "registry-duplicate", 'Registry category "labels" is already defined');
});

test("default sequence is 50 and add chains", () => {
  const r = new Registry("labels", isString);
  expect(r.add("default", "D")).toBe(r);
  r.add("before", "B", { sequence: 49 }).add("after", "A", { sequence: 51 });
  expect(r.getAll()).toEqual(["B", "D", "A"]);
});

/// (partial: typed ExtensionError code instead of DuplicatedKeyError subclass)
/// (partial: typed ExtensionError code instead of KeyNotFoundError subclass)
test("duplicate and missing entries fail by kind and exact message", () => {
  const r = new Registry("labels", isString).add("form", "Form");
  fails(() => r.add("form", "Other"), "registry-duplicate", 'Cannot add key "form" in the "labels" registry: it already exists');
  fails(() => r.get("missing"), "registry-missing", 'Cannot find key "missing" in the "labels" registry');
  expect(r.get("form")).toBe("Form");
});

test("force retains sequence, tie order is stable, snapshots cannot corrupt cache", () => {
  const r = new Registry("labels", isString);
  r.add("late", "L", { sequence: 80 }).add("early", "E", { sequence: 10 }).add("tie", "T", { sequence: 10 });
  r.add("early", "R", { force: true });
  expect(r.getEntries()).toEqual([["early", "R"], ["tie", "T"], ["late", "L"]]);
  const snapshot = r.getEntries();
  snapshot[0]![0] = "corrupt";
  snapshot.reverse();
  r.getAll().pop();
  expect(r.getEntries()).toEqual([["early", "R"], ["tie", "T"], ["late", "L"]]);
});

test("explicit falsy fallback preserves its value, including undefined", () => {
  const r = new Registry("labels", isString);
  for (const v of [null, false, 0, "", undefined]) expect(r.get("missing", v)).toBe(v);
  r.add("__proto__", "safe");
  expect(r.get("__proto__")).toBe("safe");
});

/// (partial: schema required at construction rather than later addValidation)
test("invalid registration and replacement never publish in either debug mode", () => {
  for (const debug of [true, false]) {
    const r = new Registry("labels", isString).add("key", "valid");
    createEnv({ debug });
    fails(() => r.add("key", 7 as unknown as string, { force: true }), "registry-schema", 'Invalid value for key "key" in the "labels" registry');
    fails(() => r.add("bad", "bad", { sequence: NaN }), "registry-options", 'Registry "labels": sequence must be a finite number');
    expect(r.getAll()).toEqual(["valid"]);
  }
});

/// (partial: signal-driven DOM updates; UPDATE EventBus compatibility not exposed)
test("runtime registry additions, replacements and removal update mounted DOM", async () => {
  const r = new Registry("labels", isString).add("a", "A");
  class Root extends Component { static override template = "Registry"; }
  const root = target();
  await mount(Root, root, { templates: [tpl("Registry", () => {
    const node = document.createElement("p");
    $.insert(node, () => r.getAll().join(","), null);
    return node;
  })] });
  expect(root.textContent).toBe("A");
  r.add("b", "B"); flushSync(); expect(root.textContent).toBe("A,B");
  r.add("a", "R", { force: true }); flushSync(); expect(root.textContent).toBe("R,B");
  r.remove("a"); flushSync(); expect(root.textContent).toBe("B");
});

/// (partial: factory form)
/// (partial: explicit sup snapshot instead of native super skeleton)
///
test("two addons chain in supplied load order and removing the first keeps the second", () => {
  class Counter { n = 2; value(): string { return String(this.n); } }
  const order: string[] = [];
  const undos: Array<() => void> = [];
  registerAddons([
    { id: "z_first", register(ctx) { order.push(ctx.addon); undos.push(ctx.patch(Counter, sup => ({ value() { return `A(${sup.value.call(this)})`; } }))); } },
    { id: "a_second", register(ctx) { order.push(ctx.addon); undos.push(ctx.patch(Counter, sup => ({ value() { return `B(${sup.value.call(this)})`; } }))); } },
  ], { registry: new RegistryHub() });
  try {
    expect(order).toEqual(["z_first", "a_second"]);
    expect(new Counter().value()).toBe("B(A(2))");
    undos[0]!(); expect(new Counter().value()).toBe("B(2)");
    undos[0]!(); expect(new Counter().value()).toBe("B(2)");
    undos[1]!(); expect(new Counter().value()).toBe("2");
  } finally { for (const undo of undos.reverse()) undo(); }
});

test("class accessors retain their counterpart and descriptors restore exactly", () => {
  class Base { n = 3; get amount(): number { return this.n; } set amount(v: number) { this.n = v; } }
  class Child extends Base {}
  const undo = patch(Child, sup => ({ get amount() { return Reflect.get(sup, "amount", this) as number * 2; } }));
  try {
    const c = new Child(); c.amount = 5;
    expect(c.amount).toBe(10);
    expect(Object.getOwnPropertyDescriptor(Child.prototype, "amount")?.enumerable).toBe(false);
  } finally { undo(); }
  expect(Object.hasOwn(Child.prototype, "amount")).toBe(false);
  expect(new Child().amount).toBe(3);
});

test("static methods, plain objects and symbols support reversible patching", () => {
  const sym = Symbol("method");
  const obj = { n: 4, [sym]() { return this.n; } };
  const undoObj = patch(obj, sup => ({ [sym]() { return sup[sym].call(this) + 1; } }));
  class Static { static method() { return 2; } }
  const undoStatic = patchStatic(Static, sup => ({ method() { return sup.method.call(this) * 3; } }));
  try { expect(obj[sym]()).toBe(5); expect(Static.method()).toBe(6); }
  finally { undoObj(); undoStatic(); }
  expect(obj[sym]()).toBe(4); expect(Static.method()).toBe(2);
});

/// (partial: load order and class-field limitation, setup-state lint deferred)
test("patching a prototype cannot override an own class field", () => {
  class Fields { field = "instance"; }
  const undo = patch(Fields, () => ({ field: "prototype" }));
  try { expect(new Fields().field).toBe("instance"); } finally { undo(); }
});

test("a throwing factory leaves the target unchanged", () => {
  const obj = { method() { return 1; } };
  const original = Object.getOwnPropertyDescriptors(obj);
  const cause = new Error("factory failed");
  throwsCause(() => patch(obj, () => { throw cause; }), cause);
  expect(Object.getOwnPropertyDescriptors(obj)).toEqual(original);
});

class Labels { static start() { return new Labels(); } label() { return "ready"; } }

/// (partial: typed class services instead of legacy service object definitions)
test("starts independent services in parallel and dependents immediately after their own dependencies", async () => {
  let release!: () => void;
  const held = new Promise<void>(r => { release = r; });
  const log: string[] = [];
  class Slow { static async start() { log.push("slow"); await held; log.push("slow-done"); return new Slow(); } }
  class Fast { static start() { log.push("fast"); return new Fast(); } }
  class Dep { static dependencies = [Fast]; static start(_env: Env, deps: ServiceResolver) { expect(deps.get(Fast)).toBeInstanceOf(Fast); log.push("dep"); return new Dep(); } }
  const services = new ServiceContainer().add(Slow).add(Dep).add(Fast);
  const starting = services.start(createEnv({ services }));
  await settle(); expect(log).toEqual(["slow", "fast", "dep"]);
  release(); await starting;
  expect(services.get(Dep)).toBeInstanceOf(Dep);
  expect(log).toEqual(["slow", "fast", "dep", "slow-done"]);
});

/// (partial: runtime graph validation; bundle assembly graph validation owned by UI3)
test("missing dependencies and cycles fail before any start side effect", async () => {
  let starts = 0;
  class Missing { static start() { return new Missing(); } }
  class Needs { static dependencies = [Missing]; static start() { starts++; return new Needs(); } }
  const missing = new ServiceContainer().add(Needs);
  await rejects(() => missing.start(createEnv({ services: missing })), "service-missing-dependency", "Service Needs depends on unregistered service Missing");
  class A { static dependencies = [Needs]; static start() { starts++; return new A(); } }
  Needs.dependencies = [A];
  const cycle = new ServiceContainer().add(A).add(Needs);
  await rejects(() => cycle.start(createEnv({ services: cycle })), "service-cycle", "Service dependency cycle: A -> Needs -> A");
  expect(starts).toBe(0);
});

test("self cycles, duplicate classes and aliases, unavailable services fail loudly", async () => {
  class Self { static dependencies = [Self]; static start() { return new Self(); } }
  const self = new ServiceContainer().add(Self);
  await rejects(() => self.start(createEnv({ services: self })), "service-cycle", "Service dependency cycle: Self -> Self");
  const services = new ServiceContainer().add(Labels, "labels");
  fails(() => services.add(Labels), "service-duplicate", "Service Labels is already registered");
  fails(() => services.get(Labels), "service-unavailable", "Service Labels is not available");
});

test("startup failure retains its cause and container never retries side effects", async () => {
  const cause = new Error("start broke"); let calls = 0;
  class Broken { static start(): Broken { calls++; throw cause; } }
  const services = new ServiceContainer().add(Broken);
  const env = createEnv({ services });
  await expect(services.start(env)).rejects.toMatchObject({ code: "service-start", message: "Service Broken failed to start", cause });
  await expect(services.start(env)).rejects.toMatchObject({ code: "service-start", message: "Service Broken failed to start", cause });
  expect(calls).toBe(1);
});

test("class and alias lookups resolve from an isolated frozen env during setup", async () => {
  const services = new ServiceContainer().add(Labels, "labels");
  const env = createEnv({ services, debug: true }); await services.start(env);
  let actual: Env | undefined;
  class Root extends Component {
    static override template = "Services";
    override setup() { actual = useEnv(); expect(this.env).toBe(actual); expect(useService(Labels).label()).toBe("ready"); expect(useService("labels").label()).toBe("ready"); }
  }
  await mount(Root, target(), { env, templates: [tpl("Services", () => null)] });
  expect(actual).toBe(env); expect(Object.isFrozen(actual)).toBe(true);
  expect(actual?.bus).toBeInstanceOf(EventTarget);
  expect(createEnv().services).not.toBe(services);
});

test("subenv inherits, is frozen, and does not leak to siblings or another app", async () => {
  const seen: Env[] = [];
  class Leaf extends Component { static override template = "Leaf"; override setup() { seen.push(useEnv()); } }
  class Scoped extends Component {
    static override template = "Scoped"; static override components = { Leaf };
    override setup() { useSubEnv({ context: Object.freeze({ area: "scoped" }) }); seen.push(this.env); }
  }
  class Root extends Component { static override template = "EnvRoot"; static override components = { Scoped, Leaf }; }
  const env = createEnv();
  const templates = [tpl("Leaf", () => null), tpl("Scoped", self => { const n = document.createElement("div"); $.component(n, null, self, "Leaf", {}, null, null); return n; }), tpl("EnvRoot", self => {
    const n = document.createElement("div"); $.component(n, null, self, "Scoped", {}, null, null); $.component(n, null, self, "Leaf", {}, null, null); return n;
  })];
  await mount(Root, target(), { env, templates });
  await mount(Leaf, target(), { templates });
  expect(seen[0]).toBe(seen[1]); expect(seen[2]).toBe(env); expect(seen[3]).not.toBe(env);
  expect(Object.getPrototypeOf(seen[0])).toBe(env); expect(Object.isFrozen(seen[0])).toBe(true);
  expect(seen[0]?.context).toEqual({ area: "scoped" }); expect(env.context).toEqual({});
});

async function destroyedOwnerCase(reject: boolean): Promise<void> {
  let resolve!: (v: string) => void; let fail!: (e: unknown) => void;
  const pending = new Promise<string>((r, j) => { resolve = r; fail = j; });
  let calls = 0;
  class Async {
    static asyncMethods = ["read"];
    static start() { return new Async(); }
    read() { calls++; return pending; }
  }
  const services = new ServiceContainer().add(Async); const env = createEnv({ services }); await services.start(env);
  let svc!: Async; let delivered = false;
  class Root extends Component { static override template = "Async"; override setup() { svc = useService(Async); } }
  const root = target(); await mount(Root, root, { env, templates: [tpl("Async", () => null)] });
  void svc.read().then(() => { delivered = true; }, () => { delivered = true; });
  unmount(root);
  if (reject) fail(new Error("late")); else resolve("late");
  await settle(); expect(delivered).toBe(false);
  await rejects(() => svc.read(), "service-destroyed", "Component is destroyed");
  expect(calls).toBe(1);
}

test("destroyed owners suppress late service resolve settlement and refuse new calls", () => destroyedOwnerCase(false));
test("destroyed owners suppress late service reject settlement and refuse new calls", () => destroyedOwnerCase(true));

test("hooks outside setup fail by kind and message", () => {
  fails(() => useEnv(), "hook-usage", "useEnv can only be called during setup()");
  fails(() => useSubEnv({}), "hook-usage", "useSubEnv can only be called during setup()");
  fails(() => useService(Labels), "hook-usage", "useService can only be called during setup()");
});

test("unpatch rebuild failure preserves the live chain and can be retried", () => {
  const obj = { value() { return "base"; } };
  const first = patch(obj, sup => ({ value() { return `first:${sup.value.call(this)}`; } }));
  let allow = true;
  const cause = new Error("rebuild refused");
  const second = patch(obj, sup => {
    if (!allow) throw cause;
    return { value() { return `second:${sup.value.call(this)}`; } };
  });
  try {
    allow = false;
    throwsCause(first, cause);
    expect(obj.value()).toBe("second:first:base");
    allow = true;
    first(); expect(obj.value()).toBe("second:base");
  } finally { allow = true; second(); first(); }
});

test("non-configurable properties are refused without partial mutation", () => {
  const obj = { a: 1, b: 2 };
  Object.defineProperty(obj, "b", { configurable: false });
  fails(() => patch(obj, () => ({ a: 3, b: 4 })), "patch-target", 'Cannot patch non-configurable member "b"');
  expect(obj.a).toBe(1); expect(obj.b).toBe(2);
});

test("async service facades retain private receivers, return live results and propagate live errors", async () => {
  const cause = new Error("read failed");
  class Private {
    #value = 7;
    static asyncMethods = ["read", "fail"];
    static start() { return new Private(); }
    value() { return this.#value; }
    async read() { return this.#value; }
    async fail(): Promise<never> { throw cause; }
  }
  const services = new ServiceContainer().add(Private); const env = createEnv({ services }); await services.start(env);
  let svc!: Private;
  class Root extends Component { static override template = "Private"; override setup() { svc = useService(Private); } }
  await mount(Root, target(), { env, templates: [tpl("Private", () => null)] });
  expect(svc.value()).toBe(7); expect(await svc.read()).toBe(7);
  await expect(svc.fail()).rejects.toBe(cause);
});

test("missing service lookup from setup names the unavailable service", async () => {
  const errors: unknown[] = [];
  class Root extends Component { static override template = "MissingService"; override setup() { useService(Labels); } }
  await rejects(() => mount(Root, target(), { templates: [tpl("MissingService", () => null)], onError: e => errors.push(e) }), "service-unavailable", "Service Labels is not available");
  expect(errors).toHaveLength(1);
});


test("patch refuses shared prototypes and dangerous member keys", () => {
  for (const obj of [Object.prototype, Function.prototype, Array.prototype]) {
    fails(() => patch(obj, () => ({})), "patch-target", "Cannot patch a shared built-in prototype");
  }
  for (const key of ["constructor", "prototype", "__proto__"]) {
    const obj = {};
    fails(() => patch(obj, () => Object.defineProperty({}, key, { value: 1, configurable: true })), "patch-target", `Cannot patch reserved member "${key}"`);
    expect(Object.hasOwn(obj, key)).toBe(false);
  }
});

test("registry bounds fail before mutation and replacement at capacity works", () => {
  const r = new Registry("labels", isString);
  fails(() => r.add("", "v"), "registry-options", 'Registry "labels": key must contain 1..256 code units');
  fails(() => r.add("x".repeat(257), "v"), "registry-options", 'Registry "labels": key must contain 1..256 code units');
  for (const sequence of [-1000001, 1000001, 0.5]) {
    fails(() => r.add("bad", "v", { sequence }), "registry-options", 'Registry "labels": sequence must be an integer in -1000000..1000000');
  }
  for (let i = 0; i < 4096; i++) r.add(String(i), "v");
  fails(() => r.add("overflow", "v"), "registry-limit", 'Registry "labels" exceeds 4096 entries');
  r.add("0", "replacement", { force: true });
  expect(r.get("0")).toBe("replacement"); expect(r.contains("overflow")).toBe(false);
});

test("service lifecycle, dependency shape and lazy class identity are explicit", async () => {
  const services = new ServiceContainer().add(Labels, "labels");
  await services.start(createEnv({ services }));
  fails(() => services.add(Labels), "service-state", "Cannot register services after startup begins");
  const OtherLabels = class Labels { static start() { return new this(); } };
  fails(() => services.get(OtherLabels), "service-unavailable", "Service Labels is not available");
  const duplicate = new ServiceContainer().add(Labels, "labels");
  fails(() => duplicate.add(OtherLabels as unknown as typeof Labels, "labels"), "service-duplicate", 'Service alias "labels" is already registered');
  class Bad { static dependencies = [null]; static start() { return new Bad(); } }
  const invalid = new ServiceContainer().add(Bad as unknown as typeof Labels);
  await rejects(() => invalid.start(createEnv({ services: invalid })), "service-definition", "Service Bad has an invalid dependency");
});

test("registry rejects malformed options and schemas without publishing", () => {
  fails(() => new Registry("labels", undefined as never), "registry-schema", 'Registry "labels" requires a schema');
  const r = new Registry("labels", isString);
  for (const options of [null, { force: 1 }, { unexpected: true }]) {
    fails(() => r.add("bad", "bad", options as never), "registry-options", 'Registry "labels": invalid options');
  }
  r.add("日本語", "valid", { sequence: -1000000 });
  expect(r.get("日本語")).toBe("valid");
  expect(r.getEntries()).toEqual([["日本語", "valid"]]);
});

test("service definitions reject malformed and repeated dependencies before side effects", async () => {
  let calls = 0;
  class Dep { static start() { calls++; return new Dep(); } }
  class Invalid { static dependencies: unknown = null; static start() { calls++; return new Invalid(); } }
  const invalid = new ServiceContainer().add(Dep).add(Invalid as never);
  await rejects(() => invalid.start(createEnv({ services: invalid })), "service-definition", "Service Invalid dependencies must be an array of at most 256 classes");
  class Repeated { static dependencies = [Dep, Dep]; static start() { calls++; return new Repeated(); } }
  const repeated = new ServiceContainer().add(Dep).add(Repeated);
  await rejects(() => repeated.start(createEnv({ services: repeated })), "service-definition", "Service Repeated repeats dependency Dep");
  expect(calls).toBe(0);
});

test("environments reject wrong known types instead of silently defaulting", () => {
  for (const options of [{ debug: null }, { isSmall: 1 }, { services: {} }, { bus: {} }, { context: [] }]) {
    fails(() => createEnv(options as never), "env-options", "Invalid environment options");
  }
});

test("repeated startup shares one result and declared dependencies limit resolver access", async () => {
  let calls = 0;
  class Dependency { static start() { return new Dependency(); } }
  class Caller {
    static start(_env: Env, deps: ServiceResolver) {
      calls++;
      deps.get(Dependency);
      return new Caller();
    }
  }
  const services = new ServiceContainer().add(Dependency).add(Caller);
  const env = createEnv({ services });
  const first = services.start(env);
  expect(services.start(env)).toBe(first);
  await expect(first).rejects.toMatchObject({
    code: "service-start", message: "Service Caller failed to start",
    cause: { code: "service-missing-dependency", message: "Service Caller did not declare dependency Dependency" },
  });
  await expect(services.start(env)).rejects.toMatchObject({ code: "service-start", message: "Service Caller failed to start" });
  expect(calls).toBe(1);
  fails(() => services.get(Dependency), "service-unavailable", "Service Dependency is not available");
});

test("empty service graph starts and independent applications do not share instances", async () => {
  const empty = new ServiceContainer(); await empty.start(createEnv({ services: empty }));
  const first = new ServiceContainer().add(Labels);
  const second = new ServiceContainer().add(Labels);
  await Promise.all([first.start(createEnv({ services: first })), second.start(createEnv({ services: second }))]);
  expect(first.get(Labels)).not.toBe(second.get(Labels));
});

test("addon list validates duplicates before callbacks and rejects async registration", () => {
  let calls = 0;
  const registry = new RegistryHub();
  fails(() => registerAddons([
    { id: "same", register() { calls++; } }, { id: "same", register() { calls++; } },
  ], { registry }), "addon-registration", 'Addon "same" is registered twice');
  fails(() => registerAddons([{ id: "async", register: (async () => {}) as never }], { registry }), "addon-registration", 'Addon "async" register must be synchronous');
  expect(calls).toBe(0);
});

test("static patches reject reserved keys and inherited setters restore", () => {
  class Static { static value = 2; }
  fails(() => patchStatic(Static, () => Object.defineProperty({}, "prototype", { value: {}, configurable: true })), "patch-target", 'Cannot patch reserved member "prototype"');
  const obj = { n: 1, get value() { return this.n; }, set value(v: number) { this.n = v; } };
  const undo = patch(obj, sup => ({ set value(v: number) { Reflect.set(sup, "value", v * 2, this); } }));
  obj.value = 3; expect(obj.value).toBe(6);
  undo(); obj.value = 4; expect(obj.value).toBe(4);
});

test("undefined standard subenv options preserve inherited fields", async () => {
  const env = createEnv({ debug: true }); let seen!: Env;
  class Root extends Component {
    static override template = "OptionalEnv";
    override setup() { useSubEnv({ debug: undefined, services: undefined }); seen = useEnv(); }
  }
  await mount(Root, target(), { env, templates: [tpl("OptionalEnv", () => null)] });
  expect(seen.debug).toBe(true); expect(seen.services).toBe(env.services);
});

test("mount refuses incomplete or null environments", async () => {
  class Root extends Component { static override template = "InvalidEnv"; }
  for (const env of [null, {}]) {
    await rejects(() => mount(Root, target(), { env: env as never, templates: [tpl("InvalidEnv", () => null)] }), "env-options", "Invalid environment options");
  }
});

test("malformed async declarations fail before service startup", async () => {
  let calls = 0;
  for (const asyncMethods of [null, ["read", "read"], new Array(1)]) {
    class InvalidAsync { static asyncMethods = asyncMethods; static start() { calls++; return new InvalidAsync(); } read() {} }
    const services = new ServiceContainer().add(InvalidAsync as never);
    await rejects(() => services.start(createEnv({ services })), "service-definition", "Service InvalidAsync asyncMethods must be true or unique method names");
  }
  expect(calls).toBe(0);
});

test("plain objects with a constructor property keep enumerable patch descriptors", () => {
  const obj = { constructor() {}, value() { return 1; } };
  const undo = patch(obj, sup => ({ value() { return sup.value.call(this) + 1; } }));
  try {
    expect(obj.value()).toBe(2);
    expect(Object.getOwnPropertyDescriptor(obj, "value")?.enumerable).toBe(true);
  } finally { undo(); }
});


test("addon guide public package augmentation example compiles and registers", () => {
  const registry = new PublicRegistryHub();
  const badges = registry.define("badges", (value: unknown): value is { title: string } =>
    typeof value === "object" && value !== null &&
    "title" in value && typeof value.title === "string");
  badges.add("my_addon.ready", { title: "Ready" }, { sequence: 20 });
  expect(badges.get("my_addon.ready").title).toBe("Ready");
});

test("asyncMethods true preserves synchronous values and throws while protecting actual thenables", async () => {
  const cause = new Error("synchronous failure");
  let finish!: (value: number) => void;
  const pending = new Promise<number>(resolve => { finish = resolve; });
  class Mixed {
    static readonly asyncMethods = true;
    static start() { return new Mixed(); }
    #value = 7;
    count() { return this.#value; }
    fail(): never { throw cause; }
    branch(wait: boolean): number | Promise<number> { return wait ? pending : this.#value; }
    async read() { return this.#value; }
  }
  const services = new ServiceContainer().add(Mixed);
  const env = createEnv({ services }); await services.start(env);
  let svc!: Mixed;
  class Root extends Component { static override template = "Mixed"; override setup() { svc = useService(Mixed); } }
  const root = target(); await mount(Root, root, { env, templates: [tpl("Mixed", () => null)] });
  expect(svc.count()).toBe(7);
  throwsCause(() => svc.fail(), cause);
  expect(svc.branch(false)).toBe(7);
  expect(await svc.read()).toBe(7);
  let delivered = false;
  void (svc.branch(true) as Promise<number>).then(() => { delivered = true; }, () => { delivered = true; });
  unmount(root); finish(9); await settle();
  expect(delivered).toBe(false);
  await rejects(() => svc.read(), "service-destroyed", "Component is destroyed");
});


test("hub category names and category capacity reject invalid input before publication", () => {
  for (const name of [undefined, null, 3, Symbol("name"), "", "x".repeat(257)]) {
    fails(() => new RegistryHub().define<"labels">(name as never, isString), "registry-options", "Registry category name must contain 1..256 code units");
    fails(() => new RegistryHub().category<"labels">(name as never), "registry-options", "Registry category name must contain 1..256 code units");
  }
  const hub = new RegistryHub();
  for (let i = 0; i < 256; i++) hub.define(`label_${i}`, isString);
  const first = hub.category("label_0");
  fails(() => hub.define("labels", isString), "registry-limit", "Registry hub exceeds 256 categories");
  expect(hub.category("label_0")).toBe(first);
  expect(hub.category("label_255").getAll()).toEqual([]);
});

test("maximum-depth service graph starts once per class and refuses a 257th class", async () => {
  let starts = 0;
  function make(index: number, dependencies: ServiceClass[]) {
    class Node {
      static dependencies = dependencies;
      static start() { starts++; return new Node(); }
      readonly index = index;
    }
    Object.defineProperty(Node, "name", { value: `Node${index}` });
    return Node;
  }
  const nodes: Array<ReturnType<typeof make>> = [];
  for (let i = 0; i < 256; i++) nodes.push(make(i, i === 0 ? [] : [nodes[i - 1]!]));
  const services = new ServiceContainer();
  for (const node of [...nodes].reverse()) services.add(node);
  class Extra { static start() { starts++; return new Extra(); } }
  fails(() => services.add(Extra), "service-definition", "Service container exceeds 256 classes");
  await services.start(createEnv({ services }));
  expect(starts).toBe(256);
  expect(services.get(nodes[255]!).index).toBe(255);
});

test("deep service cycle reports the cycle without its acyclic tail and starts nothing", async () => {
  let starts = 0;
  function make(index: number, dependencies: ServiceClass[]) {
    class Node {
      static dependencies = dependencies;
      static start() { starts++; return new Node(); }
    }
    Object.defineProperty(Node, "name", { value: `Tail${index}` });
    return Node;
  }
  const nodes: Array<ReturnType<typeof make>> = [];
  for (let i = 0; i < 32; i++) nodes.push(make(i, i === 0 ? [] : [nodes[i - 1]!]));
  nodes[0]!.dependencies = [nodes[16]!];
  const services = new ServiceContainer();
  for (const node of [...nodes].reverse()) services.add(node);
  const cycle = [...Array.from({ length: 17 }, (_, i) => `Tail${16 - i}`), "Tail16"].join(" -> ");
  await rejects(() => services.start(createEnv({ services })), "service-cycle", `Service dependency cycle: ${cycle}`);
  expect(starts).toBe(0);
});

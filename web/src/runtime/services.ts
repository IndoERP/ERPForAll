/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { trackTestWork } from "./test-mutations.js";
import type { Env } from "./env.js";
import { type Owner, isThenable } from "./owner.js";
import { ExtensionError } from "./extension-error.js";

/** Augment with stable string aliases and their service instance types. */
export interface Services {}
/** Canonical class identity must be imported from its defining module, including lazy bundles. */
export interface ServiceClass<T extends object = object> {
  readonly name: string;
  readonly prototype: T;
  readonly dependencies?: readonly ServiceClass[];
  readonly asyncMethods?: true | readonly string[];
  start(env: Env, dependencies: ServiceResolver): T | PromiseLike<T>;
}
/** Restricted resolver passed to start: only declared dependencies are accessible. */
export interface ServiceResolver { get<T extends object>(service: ServiceClass<T>): T; }
type Key = ServiceClass | string;
interface Definition { key: ServiceClass; dependencies: readonly ServiceClass[]; asyncMethods: true | readonly string[]; }
function label(key: Key): string { return typeof key === "string" ? key : (key as ServiceClass).name; }
function isClass(value: unknown): value is ServiceClass {
  return typeof value === "function" && typeof (value as unknown as ServiceClass).start === "function" &&
    typeof (value as unknown as ServiceClass).prototype === "object";
}

/** Per-app service graph, bounded to 256 canonical classes and 256 aliases. Startup is terminal. */
export class ServiceContainer implements ServiceResolver {
  private readonly classes = new Set<ServiceClass>();
  private readonly aliases = new Map<string, ServiceClass>();
  private readonly instances = new Map<ServiceClass, object>();
  private readonly definitions = new Map<ServiceClass, Definition>();
  private starting: Promise<void> | null = null;
  private state: "registering" | "starting" | "ready" | "failed" = "registering";

  /** Register before start; alias types must match the instance type. */
  add<T extends object>(service: ServiceClass<T>): this;
  add<K extends keyof Services>(service: ServiceClass<Services[K] & object>, alias: K): this;
  add(service: ServiceClass, alias?: string): this {
    if (this.state !== "registering") throw new ExtensionError("service-state", "Cannot register services after startup begins");
    if (!isClass(service)) throw new ExtensionError("service-definition", "Service must be a class with a static start method");
    if (this.classes.has(service)) throw new ExtensionError("service-duplicate", `Service ${service.name} is already registered`);
    if (alias !== undefined && (typeof alias !== "string" || alias.length === 0 || alias.length > 256)) throw new ExtensionError("service-definition", "Service alias must contain 1..256 code units");
    if (alias !== undefined && this.aliases.has(alias)) throw new ExtensionError("service-duplicate", `Service alias "${String(alias)}" is already registered`);
    if (this.classes.size >= 256) throw new ExtensionError("service-definition", "Service container exceeds 256 classes");
    this.classes.add(service);
    if (alias !== undefined) this.aliases.set(alias, service);
    return this;
  }
  private resolve(key: Key): ServiceClass | undefined { return typeof key === "string" ? this.aliases.get(key) : key as ServiceClass; }
  /** A service is available once its own startup completes; failure invalidates all partial instances. */
  get<T extends object>(key: ServiceClass<T>): T;
  get<K extends keyof Services>(key: K): Services[K];
  get(key: Key): object {
    const cls = this.resolve(key);
    const value = cls === undefined ? undefined : this.instances.get(cls);
    if (value === undefined || this.state === "failed") throw new ExtensionError("service-unavailable", `Service ${label(key)} is not available`);
    return value;
  }
  private validate(): void {
    for (const key of this.classes) {
      const deps = key.dependencies === undefined ? [] : key.dependencies;
      if (!Array.isArray(deps) || deps.length > 256) throw new ExtensionError("service-definition", `Service ${key.name} dependencies must be an array of at most 256 classes`);
      const seen = new Set<ServiceClass>();
      for (const dep of deps) {
        if (!isClass(dep)) throw new ExtensionError("service-definition", `Service ${key.name} has an invalid dependency`);
        if (seen.has(dep)) throw new ExtensionError("service-definition", `Service ${key.name} repeats dependency ${dep.name}`);
        seen.add(dep);
        if (!this.classes.has(dep)) throw new ExtensionError("service-missing-dependency", `Service ${key.name} depends on unregistered service ${dep.name}`);
      }
      const asyncMethods = key.asyncMethods === undefined ? [] : key.asyncMethods;
      if (asyncMethods !== true && (!Array.isArray(asyncMethods) || asyncMethods.length > 256 || Array.from(asyncMethods).some(x => typeof x !== "string" || x.length === 0 || x.length > 256) || new Set(asyncMethods).size !== asyncMethods.length)) {
        throw new ExtensionError("service-definition", `Service ${key.name} asyncMethods must be true or unique method names`);
      }
      this.definitions.set(key, { key, dependencies: [...deps], asyncMethods: asyncMethods === true ? true : [...asyncMethods] });
    }
    const visited = new Set<ServiceClass>();
    const stack: ServiceClass[] = [];
    const visit = (key: ServiceClass): void => {
      const at = stack.indexOf(key);
      if (at >= 0) throw new ExtensionError("service-cycle", `Service dependency cycle: ${[...stack.slice(at), key].map(x => x.name).join(" -> ")}`);
      if (visited.has(key)) return;
      stack.push(key);
      for (const dep of this.definitions.get(key)!.dependencies) visit(dep);
      stack.pop(); visited.add(key);
    };
    for (const key of this.classes) visit(key);
  }
  /** Validate the entire graph first; start independent nodes without a wave barrier. Repeated calls share the result. */
  start(env: Env): Promise<void> {
    if (this.starting !== null) return this.starting;
    this.state = "starting";
    this.starting = this.run(env);
    trackTestWork(this.starting);
    return this.starting;
  }
  private failed(): boolean { return this.state === "failed"; }
  private async run(env: Env): Promise<void> {
    try {
      if (env.services !== this) throw new ExtensionError("service-definition", "Service startup environment belongs to another container");
      this.validate();
      const pending = new Map<ServiceClass, Promise<void>>();
      const start = (key: ServiceClass): Promise<void> => {
        const old = pending.get(key);
        if (old !== undefined) return old;
        const def = this.definitions.get(key)!;
        const promise = Promise.all(def.dependencies.map(start)).then(async () => {
          if (this.state === "failed") return;
          const resolver: ServiceResolver = { get: <T extends object>(dep: ServiceClass<T>): T => {
            if (!def.dependencies.includes(dep)) throw new ExtensionError("service-missing-dependency", `Service ${key.name} did not declare dependency ${dep.name}`);
            return this.get(dep);
          } };
          try {
            const started = key.start(env, Object.freeze(resolver));
            const instance = await (isThenable(started) ? trackTestWork(started) : started);
            if (instance === null || typeof instance !== "object") throw new ExtensionError("service-definition", `Service ${key.name} start must return an instance`);
            if (def.asyncMethods !== true) for (const method of def.asyncMethods) {
              if (typeof Reflect.get(instance, method) !== "function") throw new ExtensionError("service-definition", `Service ${key.name} async method ${method} is not callable`);
            }
            if (!this.failed()) this.instances.set(key, instance);
          } catch (cause) {
            this.state = "failed";
            throw new ExtensionError("service-start", `Service ${key.name} failed to start`, { cause });
          }
        });
        pending.set(key, promise);
        return promise;
      };
      await Promise.all([...this.classes].map(start));
      this.state = "ready";
    } catch (error) {
      this.state = "failed";
      this.instances.clear();
      throw error;
    }
  }
  /** @internal Component-owned facade. The shell avoids Proxy invariants on frozen instances. */
  facade<T extends object>(key: ServiceClass<T> | keyof Services, owner: Owner): T {
    const instance = this.get(key as ServiceClass<T>);
    const cls = this.resolve(key)!;
    const def = this.definitions.get(cls)!;
    const methods = new Map<PropertyKey, { original: Function; wrapped: Function }>();
    return new Proxy(Object.create(Object.getPrototypeOf(instance)) as T, {
      get: (_target, property) => {
        const value: unknown = Reflect.get(instance, property, instance);
        if (typeof value !== "function") return value;
        const cached = methods.get(property);
        if (cached?.original === value) return cached.wrapped;
        const asyncMethod = def.asyncMethods === true || (typeof property === "string" && def.asyncMethods.includes(property));
        const wrapped = (...args: unknown[]): unknown => {
          if (owner.disposed) {
            const error = new ExtensionError("service-destroyed", "Component is destroyed");
            if (asyncMethod) return Promise.reject(error);
            throw error;
          }
          // Run outside a Promise executor: synchronous values and throws keep the instance API.
          const result: unknown = Reflect.apply(value, instance, args);
          if (!asyncMethod || !isThenable(result)) return result;
          return new Promise((resolve, reject) => {
            // Both late outcomes stay observed; only actual thenables need delivery protection.
            Promise.resolve(result).then(
              result => { if (!owner.disposed) resolve(result); },
              error => { if (!owner.disposed) reject(error); },
            );
          });
        };
        methods.set(property, { original: value, wrapped });
        return wrapped;
      },
      set: () => { throw new ExtensionError("service-state", "Service facades are read-only"); },
    });
  }
}

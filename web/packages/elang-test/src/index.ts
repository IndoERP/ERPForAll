/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { mount, unmount, hasMountedApp } from "../../../src/runtime/component.js";
import type { ComponentClass, MountOptions, PropsOf } from "../../../src/runtime/component.js";
import { createEnv } from "../../../src/runtime/env.js";
import { ServiceContainer, type ServiceClass } from "../../../src/runtime/services.js";
import { RegistryHub } from "../../../src/runtime/registry.js";
import { patch, patchStatic } from "../../../src/runtime/patch.js";
import { isThenable } from "../../../src/runtime/owner.js";
import { openMutationJournal } from "../../../src/runtime/test-mutations.js";

export type HarnessErrorCode = "scope-active" | "scope-closed" | "async-manage" | "leaked-patch" | "rpc-unhandled";
/** Harness errors remain distinguishable from component and service failures. */
export class HarnessError extends Error {
  constructor(readonly code: HarnessErrorCode, message: string) { super(message); this.name = "HarnessError"; }
}
/** One realm-wide mutation owner; tests using it must not run concurrently. */
export class TestScope {
  private readonly journal;
  private closed = false;
  private disposal: Promise<void> | undefined;
  private invokingTeardown = false;
  private readonly roots = new Set<HTMLElement>();
  readonly services = new ServiceContainer();
  readonly registries = new RegistryHub();
  readonly env = createEnv({ services: this.services });
  readonly patch: typeof patch;
  readonly patchStatic: typeof patchStatic;
  constructor() {
    try { this.journal = openMutationJournal(); }
    catch (cause) { throw new HarnessError("scope-active", (cause as Error).message); }
    this.patch = ((...args: Parameters<typeof patch>) => {
      this.ready();
      return this.journal.manage(() => Reflect.apply(patch, undefined, args));
    }) as typeof patch;
    this.patchStatic = ((...args: Parameters<typeof patchStatic>) => {
      this.ready();
      return this.journal.manage(() => Reflect.apply(patchStatic, undefined, args));
    }) as typeof patchStatic;
  }
  private ready(): void { if (this.closed) throw new HarnessError("scope-closed", "Component test scope is closed"); }
  /** Own synchronous addon registration; thenable returns are rejected, observed and drained at disposal. */
  manage<T>(fn: () => T, ..._syncOnly: [Extract<T, PromiseLike<unknown>>] extends [never] ? [] : [never]): T {
    this.ready();
    if (Object.prototype.toString.call(fn) === "[object AsyncFunction]") throw new HarnessError("async-manage", "Test scope manage requires a synchronous callback");
    return this.journal.manage(() => {
      const result = fn();
      if (isThenable(result)) {
        this.journal.track(Promise.resolve(result), true);
        throw new HarnessError("async-manage", "Test scope manage requires a synchronous callback");
      }
      return result;
    });
  }
  /** Replace startup output while retaining the canonical service key and graph validation. */
  mockService<T extends object>(service: ServiceClass<T>, instance: T): void {
    this.ready();
    this.manage(() => { patchStatic(service as ServiceClass<T> & Function, () => ({ start: () => instance })); });
    this.services.add(service);
  }
  /** Mount with the real runtime in an attached, scope-owned target. */
  async mount<C extends ComponentClass>(Root: C, options: Omit<MountOptions<PropsOf<C>>, "env">): Promise<{ root: HTMLElement; instance: InstanceType<C> }> {
    this.ready();
    const root = document.createElement("div"); document.body.appendChild(root); this.roots.add(root);
    try {
      await this.services.start(this.env);
      this.ready();
      // Only env changes; TS cannot distribute Omit over the generic conditional props field.
      const instance = await mount(Root, root, { ...options, env: this.env } as MountOptions<PropsOf<C>>);
      this.ready();
      return { root, instance };
    } catch (cause) {
      const errors: unknown[] = [cause];
      // Service startup can fail before runtime mount registered the target.
      if (hasMountedApp(root)) { try { unmount(root); } catch (error) { errors.push(error); } }
      root.remove(); this.roots.delete(root);
      if (errors.length > 1) throw new AggregateError(errors, "Component mount and cleanup failed");
      throw cause;
    }
  }
  /** Share teardown for ordinary callers; extra calls while teardown callbacks are pending reject to prevent self-wait cycles. */
  dispose(): Promise<void> {
    if (this.invokingTeardown) return Promise.reject(new HarnessError("scope-closed", "Cannot dispose a component test scope from its teardown callbacks"));
    if (this.journal.hasPendingTeardown()) return Promise.reject(new HarnessError("scope-closed", "Cannot dispose a component test scope while teardown callbacks are pending"));
    if (this.disposal !== undefined) return this.disposal;
    this.closed = true;
    // Publish the cleanup promise before any component teardown can reenter disposal.
    this.disposal = Promise.resolve().then(() => this.cleanup());
    return this.disposal;
  }
  private async cleanup(): Promise<void> {
    const errors: unknown[] = [];
    this.invokingTeardown = true;
    try {
      for (const root of [...this.roots].reverse()) { try { if (hasMountedApp(root)) unmount(root); } catch (error) { errors.push(error); } finally { root.remove(); } }
      this.roots.clear();
    } finally { this.invokingTeardown = false; }
    errors.push(...await this.journal.drain());
    const leaked = this.journal.patches.filter(p => p.active && !p.managed).flatMap(p => p.members.map(String));
    if (leaked.length) errors.push(new HarnessError("leaked-patch", `Unmanaged patches remain at test teardown: ${leaked.join(", ")}`));
    try { this.journal.close(); } catch (error) { errors.push(error); }
    if (errors.length === 1) throw errors[0];
    if (errors.length > 1) throw new AggregateError(errors, "Component test cleanup failed");
  }
}
/** Open an isolated component scope; overlap rejects with scope-active. Always await disposal. */
export function createTestScope(): TestScope { return new TestScope(); }
/** Execute a test body, preserving its failure alongside any teardown failure. */
export async function withTestScope<T>(body: (scope: TestScope) => T | Promise<T>): Promise<T> {
  const scope = createTestScope();
  let value: T | undefined;
  const errors: unknown[] = [];
  try { value = await body(scope); } catch (error) { errors.push(error); }
  try { await scope.dispose(); } catch (error) { errors.push(error); }
  if (errors.length === 1) throw errors[0];
  if (errors.length > 1) throw new AggregateError(errors, "Component test body and cleanup failed");
  return value as T;
}
type Operation = { request: unknown; response: unknown };
type ContractShape<C> = { [K in keyof C]: Operation };
/** Caller-owned operation contracts import their DTOs from the generated schema owner. */
export class MockRpc<C extends ContractShape<C>> {
  private readonly handlers = new Map<keyof C, (request: never) => unknown>();
  private readonly recorded: Array<{ operation: keyof C; request: unknown }> = [];
  /** Ordered copied call records; request payloads retain their original identity. */
  get calls(): ReadonlyArray<{ readonly operation: keyof C; readonly request: unknown }> { return [...this.recorded]; }
  /** Install or replace an explicit operation handler, checked against the caller-owned DTO contract. */
  on<K extends keyof C>(operation: K, handler: (request: C[K]["request"]) => C[K]["response"] | Promise<C[K]["response"]>): this {
    this.handlers.set(operation, handler); return this;
  }
  /** Record and dispatch one call; missing handlers reject with rpc-unhandled, handler failures propagate. */
  async call<K extends keyof C>(operation: K, request: C[K]["request"]): Promise<C[K]["response"]> {
    this.recorded.push({ operation, request });
    const handler = this.handlers.get(operation);
    if (handler === undefined) throw new HarnessError("rpc-unhandled", `No mock RPC handler for "${String(operation)}"`);
    return await handler(request as never) as C[K]["response"];
  }
}

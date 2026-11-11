/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ExtensionError } from "./extension-error.js";
import { mutationJournal, type PatchMutation } from "./test-mutations.js";

/** Side-effect-free descriptor factory; it may run again when an earlier patch is removed. */
// Keep generic/overloaded signatures while checking the last signature contravariantly.
type Member<T> = T extends (...args: infer A) => infer R ? T & ((...args: A) => R) : T;
type Members<T> = { [K in keyof T as K extends "constructor" | "prototype" | "__proto__" ? never : K]?: Member<T[K]> };
export type PatchFactory<T extends object> = (sup: Readonly<T>) => Members<T> & ThisType<T>;
type CheckedFactory<T extends object, P extends Members<T>> = (sup: Readonly<T>) => P & ThisType<T>;
type Exact<T, P> = [P] extends [never] ? [] : Exclude<keyof P, keyof Members<T>> extends never ? [] : [unknownMembers: never];
/** Idempotent removal; a rebuild error preserves the current chain and can be retried. */
export type Unpatch = () => void;
type Factory = (sup: object) => object;
interface Layer { factory: Factory; members: PropertyKey[]; }
interface Handle { unpatch: Unpatch; target: object; layer: Layer; mutation?: PatchMutation; }
interface Chain {
  base: PropertyDescriptorMap;
  layers: Layer[];
  keys: Set<PropertyKey>;
  classPrototype: boolean;
  busy: boolean;
}
const chains = new WeakMap<object, Chain>();

function descriptor(obj: object | null, key: PropertyKey): PropertyDescriptor | undefined {
  while (obj !== null) {
    const d = Object.getOwnPropertyDescriptor(obj, key);
    if (d !== undefined) return d;
    obj = Object.getPrototypeOf(obj) as object | null;
  }
  return undefined;
}
function shared(target: object): boolean {
  return target === Object.prototype || target === Function.prototype || target === Array.prototype;
}
function reserved(key: PropertyKey): boolean { return key === "constructor" || key === "prototype" || key === "__proto__"; }

// Build every replacement in isolation before publishing. Only managed keys touch the live object.
function rebuild(target: object, chain: Chain, layers: Layer[]): void {
  const staged: object = Object.create(Object.getPrototypeOf(target));
  Object.defineProperties(staged, chain.base);
  const nextKeys = new Set<PropertyKey>();
  const members = new Map<Layer, PropertyKey[]>();
  for (const layer of layers) {
    const sup: object = Object.create(Object.getPrototypeOf(staged));
    Object.defineProperties(sup, Object.getOwnPropertyDescriptors(staged));
    const extension = layer.factory(sup);
    if (extension === null || typeof extension !== "object") throw new ExtensionError("patch-target", "Patch factory must return an object");
    members.set(layer, Reflect.ownKeys(extension));
    for (const key of Reflect.ownKeys(extension)) {
      if (reserved(key)) throw new ExtensionError("patch-target", `Cannot patch reserved member "${String(key)}"`);
      const prior = descriptor(staged, key);
      const own = Object.getOwnPropertyDescriptor(target, key);
      if (own?.configurable === false) throw new ExtensionError("patch-target", `Cannot patch non-configurable member "${String(key)}"`);
      const d = Object.getOwnPropertyDescriptor(extension, key)!;
      if (d.configurable !== true) throw new ExtensionError("patch-target", `Patch member "${String(key)}" must be configurable`);
      if (!("value" in d) && prior !== undefined && !("value" in prior)) {
        d.get ??= prior.get;
        d.set ??= prior.set;
      }
      if (chain.classPrototype) d.enumerable = false;
      if (!Object.hasOwn(target, key) && !Object.isExtensible(target)) {
        throw new ExtensionError("patch-target", `Cannot add member "${String(key)}" to a non-extensible target`);
      }
      Object.defineProperty(staged, key, d);
      nextKeys.add(key);
    }
  }
  const changed = new Set([...chain.keys, ...nextKeys]);
  const previous = new Map<PropertyKey, PropertyDescriptor | undefined>();
  for (const key of changed) {
    const own = Object.getOwnPropertyDescriptor(target, key);
    if (own?.configurable === false) throw new ExtensionError("patch-target", `Cannot patch non-configurable member "${String(key)}"`);
    previous.set(key, own);
  }
  try {
    for (const key of changed) {
      const d = Object.getOwnPropertyDescriptor(staged, key);
      if (d === undefined) {
        if (!Reflect.deleteProperty(target, key)) throw new ExtensionError("patch-target", `Cannot remove member "${String(key)}"`);
      } else Object.defineProperty(target, key, d);
    }
  } catch (cause) {
    const errors: unknown[] = [cause];
    for (const [key, d] of previous) {
      try {
        if (d !== undefined) Object.defineProperty(target, key, d);
        else if (!Reflect.deleteProperty(target, key)) throw new Error(`Cannot roll back member "${String(key)}"`);
      } catch (error) { errors.push(error); }
    }
    if (errors.length > 1) throw new AggregateError(errors, "Patch publication and rollback failed");
    throw cause;
  }
  chain.keys = nextKeys;
  chain.layers = layers;
  for (const [layer, keys] of members) layer.members = keys;
}
function apply(target: object, factory: Factory, classPrototype: boolean): Handle {
  if ((typeof target !== "object" && typeof target !== "function") || target === null) throw new ExtensionError("patch-target", "Patch target must be an object or class");
  if (shared(target)) throw new ExtensionError("patch-target", "Cannot patch a shared built-in prototype");
  if (typeof factory !== "function") throw new ExtensionError("patch-target", "Patch requires a factory function");
  const chain = chains.get(target) ?? { base: Object.getOwnPropertyDescriptors(target), layers: [], keys: new Set(), classPrototype, busy: false };
  if (chain.busy) throw new ExtensionError("patch-target", "Cannot mutate a patch chain from its factory");
  const layer: Layer = { factory, members: [] };
  chain.busy = true;
  chains.set(target, chain);
  try { rebuild(target, chain, [...chain.layers, layer]); }
  finally { chain.busy = false; if (chain.layers.length === 0) chains.delete(target); }
  let removed = false;
  let mutation: PatchMutation | undefined;
  const unpatch = () => {
    if (removed) return;
    if (chain.busy) throw new ExtensionError("patch-target", "Cannot mutate a patch chain from its factory");
    chain.busy = true;
    try {
      rebuild(target, chain, chain.layers.filter(x => x !== layer));
      removed = true;
      if (mutation !== undefined) mutation.active = false;
      if (chain.layers.length === 0) chains.delete(target);
    } finally { chain.busy = false; }
  };
  const journal = mutationJournal();
  if (journal !== undefined) {
    journal.patch({ members: [...layer.members], active: true, undo: unpatch });
    mutation = journal.patches[journal.patches.length - 1];
  }
  return { unpatch, target, layer, mutation };
}

/** Patch instance members of a class, or own members of a plain object. */
export function patch<T extends object, P extends Members<NoInfer<T>>>(target: abstract new (...args: never[]) => T, factory: CheckedFactory<NoInfer<T>, P>, ...exact: Exact<T, P>): Unpatch;
export function patch<T extends object, P extends Members<NoInfer<T>>>(target: T & (T extends Function ? never : unknown), factory: CheckedFactory<NoInfer<T>, P>, ...exact: Exact<T, P>): Unpatch;
export function patch(target: object, factory: Factory): Unpatch {
  return install(target, factory, false).unpatch;
}
function install(target: object, factory: Factory, isStatic: boolean): Handle {
  if (isStatic) {
    if (typeof target !== "function") throw new ExtensionError("patch-target", "patchStatic requires a class");
    return apply(target, factory, false);
  }
  if (shared(target)) throw new ExtensionError("patch-target", "Cannot patch a shared built-in prototype");
  const isClass = typeof target === "function";
  const actual = isClass ? (target as { prototype: object }).prototype : target;
  if (actual === null || (typeof actual !== "object" && typeof actual !== "function")) throw new ExtensionError("patch-target", "Patch target must be an object or class");
  const constructor = Object.getOwnPropertyDescriptor(actual, "constructor")?.value as unknown;
  const classPrototype = isClass || (typeof constructor === "function" && constructor.prototype === actual);
  return apply(actual, factory, classPrototype);
}
/** Patch constructor/static members; instance members use patch(Class, factory). */
export function patchStatic<T extends Function, P extends Members<NoInfer<T>>>(target: T, factory: CheckedFactory<NoInfer<T>, P>, ..._exact: Exact<T, P>): Unpatch {
  return install(target, factory as Factory, true).unpatch;
}

/** Active patch provenance. Target is the actual patched object (a class's prototype for instances). */
export interface PatchInfo { readonly addon: string; readonly target: object; readonly member: PropertyKey; }

/** Own at most 4096 active patches for one addon; remove them independently or in reverse order. */
export class PatchSet {
  private readonly handles = new Set<Handle>();
  private busy = false;
  private closed = false;
  readonly addon: string;
  /** Bound functions retain the same checked overloads as the standalone patch APIs. */
  readonly patch: typeof patch;
  readonly patchStatic: typeof patchStatic;
  constructor(addon: string) {
    if (typeof addon !== "string" || addon.length === 0 || addon.length > 256) {
      throw new ExtensionError("addon-registration", "PatchSet addon must contain 1..256 code units");
    }
    this.addon = addon;
    this.patch = ((target: object, factory: Factory) => this.add(target, factory, false)) as typeof patch;
    this.patchStatic = ((target: object, factory: Factory) => this.add(target, factory, true)) as typeof patchStatic;
  }
  private idle(): void {
    if (this.busy) throw new ExtensionError("patch-target", "Cannot mutate a PatchSet from its factory");
  }
  private add(target: object, factory: Factory, isStatic: boolean): Unpatch {
    this.idle();
    if (this.closed) throw new ExtensionError("patch-target", `PatchSet "${this.addon}" is closed`);
    if (typeof factory !== "function") throw new ExtensionError("patch-target", "Patch requires a factory function");
    if (this.handles.size >= 4096) throw new ExtensionError("patch-target", `PatchSet "${this.addon}" exceeds 4096 patches`);
    this.busy = true;
    let handle: Handle;
    const guarded: Factory = sup => {
      // Another addon's removal may rebuild our layer while this set is otherwise idle.
      const busy = this.busy;
      this.busy = true;
      try { return factory(sup); } finally { this.busy = busy; }
    };
    try {
      handle = install(target, guarded, isStatic); this.handles.add(handle);
      // Journal cleanup must release the owning set, not only its descriptor layer.
      if (handle.mutation !== undefined) handle.mutation.undo = () => this.remove(handle);
    }
    finally { this.busy = false; }
    return () => this.remove(handle);
  }
  private remove(handle: Handle): void {
    this.idle();
    if (!this.handles.has(handle)) return;
    this.busy = true;
    try { handle.unpatch(); this.handles.delete(handle); }
    finally { this.busy = false; }
  }
  /** Copy current provenance without running factories; symbols keep their identity. */
  getPatches(): PatchInfo[] {
    return [...this.handles].flatMap(h => h.layer.members.map(member => ({ addon: this.addon, target: h.target, member })));
  }
  /** Close the set and try every removal; failures remain retryable and throw AggregateError. */
  unpatchAll(): void {
    this.idle();
    this.closed = true;
    const errors: unknown[] = [];
    for (const handle of [...this.handles].reverse()) {
      try { this.remove(handle); } catch (error) { errors.push(error); }
    }
    if (errors.length > 0) throw new AggregateError(errors, `Cannot remove all patches for addon "${this.addon}"`);
  }
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { mutationJournal } from "./test-mutations.js";
import { signal } from "../../vendor/alien-signals/src/index.js";
import { ExtensionError } from "./extension-error.js";
import { untrack } from "./reactive.js";

/** Augment with category names and their value types from addon modules. */
export interface Registries {}
/** Required runtime validator, executed in every mode. */
export type RegistrySchema<T> = (value: unknown) => value is T;
/** Replacement is opt-in; sequence must be an integer in -1000000..1000000. */
export interface RegistryOptions { readonly force?: boolean; readonly sequence?: number; }
interface Entry<T> { value: T; sequence: number; }

function validName(name: unknown): name is string {
  return typeof name === "string" && name.length > 0 && name.length <= 256;
}

/** Bounded reactive category: at most 4096 entries with keys of 1..256 code units. */
export class Registry<T> {
  private readonly entries = new Map<string, Entry<T>>();
  private readonly revision = signal<object>({});
  private sorted: Array<[string, T]> | null = null;
  constructor(readonly name: string, private readonly schema: RegistrySchema<T>) {
    if (!validName(name)) throw new ExtensionError("registry-options", "Registry name must contain 1..256 code units");
    if (typeof schema !== "function") throw new ExtensionError("registry-schema", `Registry "${name}" requires a schema`);
  }
  private key(key: string): void {
    if (!validName(key)) throw new ExtensionError("registry-options", `Registry "${this.name}": key must contain 1..256 code units`);
  }
  private snapshot(): void {
    const journal = mutationJournal();
    if (journal === undefined) return;
    journal.snapshot(this, () => {
      const entries = new Map(this.entries);
      return () => { this.entries.clear(); for (const [key, value] of entries) this.entries.set(key, value); this.changed(); };
    });
  }
  private changed(): void { this.sorted = null; this.revision({}); }
  /** Insert a validated value. Duplicate keys need force; replacement retains sequence. */
  add(key: string, value: T, options: RegistryOptions = {}): this {
    this.key(key);
    if (options === null || typeof options !== "object" || Reflect.ownKeys(options).some(k => k !== "force" && k !== "sequence") ||
      (options.force !== undefined && typeof options.force !== "boolean")) {
      throw new ExtensionError("registry-options", `Registry "${this.name}": invalid options`);
    }
    const sequence = options.sequence;
    if (sequence !== undefined && (typeof sequence !== "number" || !Number.isFinite(sequence))) {
      throw new ExtensionError("registry-options", `Registry "${this.name}": sequence must be a finite number`);
    }
    if (sequence !== undefined && (!Number.isInteger(sequence) || sequence < -1_000_000 || sequence > 1_000_000)) {
      throw new ExtensionError("registry-options", `Registry "${this.name}": sequence must be an integer in -1000000..1000000`);
    }
    const old = this.entries.get(key);
    if (old !== undefined && options.force !== true) throw new ExtensionError("registry-duplicate", `Cannot add key "${key}" in the "${this.name}" registry: it already exists`);
    if (old === undefined && this.entries.size >= 4096) throw new ExtensionError("registry-limit", `Registry "${this.name}" exceeds 4096 entries`);
    let valid: boolean;
    try { valid = untrack(() => this.schema(value)); }
    catch (cause) { throw new ExtensionError("registry-schema", `Invalid value for key "${key}" in the "${this.name}" registry`, { cause }); }
    if (valid !== true) throw new ExtensionError("registry-schema", `Invalid value for key "${key}" in the "${this.name}" registry`);
    this.snapshot();
    this.entries.set(key, { value, sequence: sequence ?? old?.sequence ?? 50 });
    this.changed();
    return this;
  }
  /** Lookup tracks mutations. Supplying a second argument opts into a fallback, even undefined. */
  get(key: string): T;
  get<F>(key: string, fallback: F): T | F;
  get<F>(key: string, fallback?: F): T | F | undefined {
    this.key(key); this.revision();
    const entry = this.entries.get(key);
    if (entry !== undefined) return entry.value;
    if (arguments.length >= 2) return fallback;
    throw new ExtensionError("registry-missing", `Cannot find key "${key}" in the "${this.name}" registry`);
  }
  /** Reactive membership query. */
  contains(key: string): boolean { this.key(key); this.revision(); return this.entries.has(key); }
  /** Delete an entry; absent keys leave the revision unchanged. */
  remove(key: string): void { this.key(key); if (this.entries.has(key)) { this.snapshot(); this.entries.delete(key); this.changed(); } }
  /** Ordered copied tuples. Payloads retain their original identity. Ties keep insertion order. */
  getEntries(): Array<[string, T]> {
    this.revision();
    this.sorted ??= [...this.entries].sort((a, b) => a[1].sequence - b[1].sequence).map(([key, entry]) => [key, entry.value]);
    return this.sorted.map(([key, value]) => [key, value]);
  }
  /** Ordered copied values; reading participates in reactive bindings. */
  getAll(): T[] { return this.getEntries().map(([, value]) => value); }
}

/** Typed schema declarations, cached per hub, capped at 256 categories. */
export class RegistryHub {
  private readonly categories = new Map<keyof Registries, Registry<unknown>>();
  /** Define a category once. An existing name is never silently replaced. */
  define<K extends keyof Registries & string>(name: K, schema: RegistrySchema<Registries[K]>): Registry<Registries[K]> {
    if (!validName(name)) throw new ExtensionError("registry-options", "Registry category name must contain 1..256 code units");
    if (this.categories.has(name)) throw new ExtensionError("registry-duplicate", `Registry category "${String(name)}" is already defined`);
    if (this.categories.size >= 256) throw new ExtensionError("registry-limit", "Registry hub exceeds 256 categories");
    const registry = new Registry(name, schema);
    mutationJournal()?.snapshot(this, () => {
      const categories = new Map(this.categories);
      return () => { this.categories.clear(); for (const [key, value] of categories) this.categories.set(key, value); };
    });
    this.categories.set(name, registry);
    return registry;
  }
  /** Resolve a previously declared typed category; no implicit unvalidated categories. */
  category<K extends keyof Registries & string>(name: K): Registry<Registries[K]> {
    if (!validName(name)) throw new ExtensionError("registry-options", "Registry category name must contain 1..256 code units");
    const registry = this.categories.get(name);
    if (registry === undefined) throw new ExtensionError("registry-missing", `Registry category "${String(name)}" is not defined`);
    return registry as Registry<Registries[K]>;
  }
}

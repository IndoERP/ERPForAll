/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ExtensionError } from "./extension-error.js";
import { reportUnhandled } from "./reactive.js";
import { isThenable } from "./owner.js";
import { PatchSet, type patch, type patchStatic } from "./patch.js";
import type { RegistryHub } from "./registry.js";
/** Registration capabilities; supplied in server assembler load order. */
export interface RegistrationContext {
  readonly addon: string;
  readonly registry: RegistryHub;
  readonly patch: typeof patch;
  readonly patchStatic: typeof patchStatic;
}
/** Synchronous addon entry point; no registration from import side effects. */
export interface AddonRegistration { readonly id: string; register(context: RegistrationContext): void | null; }
/** Registration and cleanup both failed. Original cause and closed, retryable patch sets remain accessible. */
export class AddonRegistrationError extends AggregateError {
  override readonly name = "AddonRegistrationError";
  private readonly sets: ReadonlyMap<string, PatchSet>;
  constructor(errors: readonly unknown[], cause: unknown, sets: ReadonlyMap<string, PatchSet>) {
    super(errors, "Addon registration and patch cleanup failed", { cause });
    this.sets = new Map(sets);
  }
  /** Copy the set index; each set retains the actual failed handles for explicit retry. */
  get patchSets(): ReadonlyMap<string, PatchSet> { return new Map(this.sets); }
}
/** Invoke server order and return addon ownership. Failure removes patches; registry side effects need app rebuild. */
export function registerAddons(addons: readonly AddonRegistration[], options: { readonly registry: RegistryHub }): ReadonlyMap<string, PatchSet> {
  if (!Array.isArray(addons) || addons.length > 4096) throw new ExtensionError("addon-registration", "Addon registrations must be an array of at most 4096 entries");
  const seen = new Set<string>();
  for (const addon of addons) {
    if (addon === null || typeof addon !== "object" || typeof addon.id !== "string" || addon.id.length === 0 || addon.id.length > 256 || typeof addon.register !== "function") {
      throw new ExtensionError("addon-registration", "Addon registration requires an id and register function");
    }
    if (seen.has(addon.id)) throw new ExtensionError("addon-registration", `Addon "${addon.id}" is registered twice`);
    seen.add(addon.id);
  }
  const sets = new Map<string, PatchSet>();
  try {
    for (const addon of addons) {
      const set = new PatchSet(addon.id);
      sets.set(addon.id, set);
      const result: unknown = addon.register(Object.freeze({ addon: addon.id, registry: options.registry, patch: set.patch, patchStatic: set.patchStatic }));
      if (isThenable(result)) {
        // Observe late rejection; closed sets also refuse any late patch installations.
        Promise.resolve(result).catch(reportUnhandled);
        throw new ExtensionError("addon-registration", `Addon "${addon.id}" register must be synchronous`);
      }
      if (result !== undefined && result !== null) throw new ExtensionError("addon-registration", `Addon "${addon.id}" register must return void or null`);
    }
  } catch (cause) {
    const errors: unknown[] = [cause];
    for (const set of [...sets.values()].reverse()) {
      try { set.unpatchAll(); } catch (error) { errors.push(error); }
    }
    if (errors.length > 1) throw new AddonRegistrationError(errors, cause, sets);
    throw cause;
  }
  return sets;
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expectTypeOf, test } from "vitest";
import { PatchSet, patch, patchStatic, RegistryHub, ServiceContainer, type Unpatch } from "../src/index.js";

export class Target {
  value = 1;
  method(n: number): string { return String(n); }
  static method(n: number): number { return n; }
  get label(): string { return "label"; }
}
// Declaration merging is how addons declare new members before patching them.
export interface Target { added(): boolean; }
class LabelService { static start(): LabelService { return new LabelService(); } label(): string { return "label"; } }
class Generic {
  identity<T>(value: T): T { return value; }
  overloaded(value: number): string;
  overloaded(value: string): number;
  overloaded(value: number | string): string | number { return typeof value === "number" ? String(value) : value.length; }
}
declare module "@erpforall/elang" {
  interface Registries { ui4Numbers: number; }
  interface Services { ui4Labels: LabelService; }
}

/** Type-checked, never executed: negative examples must be rejected by tsc. */
function typeOnly(): void {
  const set = new PatchSet("typed");
  for (const apply of [patch, set.patch]) {
    const undo = apply(Target, sup => ({ method(n) {
      expectTypeOf(n).toEqualTypeOf<number>(); expectTypeOf(this).toEqualTypeOf<Target>();
      return sup.method.call(this, n) + String(this.value);
    }, added() { return true; } }));
    expectTypeOf(undo).toEqualTypeOf<Unpatch>();
    // @ts-expect-error override result must stay string
    apply(Target, () => ({ method(n: number) { return n; } }));
    // @ts-expect-error override parameter must stay number
    apply(Target, () => ({ method(n: string) { return n; } }));
    // @ts-expect-error a narrower parameter does not implement all predecessor calls
    apply(Target, () => ({ method(n: 1) { return String(n); } }));
    // @ts-expect-error mixed known/unknown members require declaration augmentation
    apply(Target, () => ({ method(n: number) { return String(n); }, unknown() {} }));
    apply(Target, sup => {
      // @ts-expect-error predecessor snapshot cannot be written
      sup.value = 2;
      // @ts-expect-error unknown predecessor member
      sup.unknown();
      return { get label() { return "new"; } };
    });
    // @ts-expect-error accessor must keep its property type
    apply(Target, () => ({ get label() { return 1; } }));
  }
  for (const apply of [patchStatic, set.patchStatic]) {
    apply(Target, sup => ({ method(n) { expectTypeOf(n).toEqualTypeOf<number>(); return sup.method.call(this, n); } }));
    // @ts-expect-error static result remains number
    apply(Target, () => ({ method(n: number) { return String(n); } }));
    // @ts-expect-error unknown statics require augmentation
    apply(Target, () => ({ method(n: number) { return n; }, unknown() {} }));
  }
  const sym = Symbol("method"); const obj = { n: 1, [sym](n: number) { return n; } };
  set.patch(obj, sup => ({ [sym](n) { return sup[sym].call(this, n) + this.n; } }));
  // @ts-expect-error object override changes value type
  set.patch(obj, () => ({ n: "bad" }));
  // @ts-expect-error a monomorphic replacement cannot implement a generic identity
  patch(Generic, () => ({ identity(value: unknown) { return value; } }));
  // @ts-expect-error an overload set must retain all signatures, not just the last
  patch(Generic, () => ({ overloaded(value: string) { return value.length; } }));
  patch(Generic, sup => ({ identity: sup.identity, overloaded: sup.overloaded }));
  // @ts-expect-error static targets must be constructor functions
  patchStatic(obj, () => ({ n: 2 }));
  const hub = new RegistryHub();
  hub.define("ui4Numbers", (v): v is number => typeof v === "number").add("n", 2);
  // @ts-expect-error schemas must match augmented categories
  hub.define("ui4Numbers", (v): v is string => typeof v === "string");
  // @ts-expect-error entries retain augmented value type
  hub.category("ui4Numbers").add("n", "bad");
  const services = new ServiceContainer(); services.add(LabelService, "ui4Labels");
  expectTypeOf(services.get(LabelService)).toEqualTypeOf<LabelService>();
  expectTypeOf(services.get("ui4Labels")).toEqualTypeOf<LabelService>();
  class WrongService { static start(): object { return {}; } }
  // @ts-expect-error a service alias keeps its augmented instance type
  services.add(WrongService, "ui4Labels");
  // @ts-expect-error unknown aliases cannot be looked up
  services.get("ui4Unknown");
}
test("extension public types are checked by tsc", () => { void typeOnly; });

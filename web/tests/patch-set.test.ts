/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { AddonRegistrationError, ExtensionError, PatchSet, RegistryHub, registerAddons, type RegistrationContext } from "../src/index.js";

function fails(fn: () => unknown, code: string, message: string): void {
  let error: unknown;
  try { fn(); } catch (e) { error = e; }
  expect(error).toBeInstanceOf(ExtensionError);
  expect(error).toMatchObject({ code, message });
}

/// (partial: load order, per-addon removal, idempotence and copied provenance)
test("addon sets remove a lower layer without losing another addon, and expose copied metadata", () => {
  class Counter { value(): string { return "base"; } }
  const sets = registerAddons([
    { id: "first", register(ctx) { ctx.patch(Counter, sup => ({ value() { return `A(${sup.value.call(this)})`; } })); } },
    { id: "second", register(ctx) { ctx.patch(Counter, sup => ({ value() { return `B(${sup.value.call(this)})`; } })); } },
  ], { registry: new RegistryHub() });
  expect(new Counter().value()).toBe("B(A(base))");
  const first = sets.get("first")!;
  const snapshot = first.getPatches();
  expect(snapshot).toEqual([{ addon: "first", target: Counter.prototype, member: "value" }]);
  snapshot.pop();
  expect(first.getPatches()).toHaveLength(1);
  first.unpatchAll(); first.unpatchAll();
  expect(new Counter().value()).toBe("B(base)");
  expect(first.getPatches()).toEqual([]);
  sets.get("second")!.unpatchAll();
  expect(new Counter().value()).toBe("base");
});

/// (partial: metadata for symbols, accessors, statics and object targets)
test("set tracks symbols, accessors, static and object targets without re-running factories to inspect", () => {
  const key = Symbol("m");
  const obj = { [key]() { return 1; }, get amount() { return 2; } };
  class Static { static value(): number { return 3; } }
  const set = new PatchSet("symbols"); let calls = 0;
  const undo = set.patch(obj, sup => { calls++; return { [key]() { return sup[key].call(this) + 1; }, get amount() { return 4; } }; });
  set.patchStatic(Static, sup => ({ value() { return sup.value.call(this) + 1; } }));
  expect(set.getPatches()).toEqual([
    { addon: "symbols", target: obj, member: "amount" },
    { addon: "symbols", target: obj, member: key },
    { addon: "symbols", target: Static, member: "value" },
  ]);
  expect(calls).toBe(1);
  undo(); undo();
  expect(set.getPatches()).toEqual([{ addon: "symbols", target: Static, member: "value" }]);
  set.unpatchAll();
  expect(obj[key]()).toBe(1); expect(Static.value()).toBe(3);
});

/// (partial: failed cleanup, retained ownership and retry)
test("failed bulk removal retains failed ownership for retry while cleaning other targets", () => {
  const obj = { value() { return "base"; } }; const other = { n: 1 };
  const lower = new PatchSet("lower"); const upper = new PatchSet("upper");
  lower.patch(other, () => ({ n: 2 }));
  lower.patch(obj, sup => ({ value() { return `A(${sup.value.call(this)})`; } }));
  let refuse = false;
  upper.patch(obj, sup => { if (refuse) throw new Error("rebuild refused"); return { value() { return `B(${sup.value.call(this)})`; } }; });
  refuse = true;
  let error: unknown;
  try { lower.unpatchAll(); } catch (e) { error = e; }
  expect(error).toBeInstanceOf(AggregateError);
  expect(error).toMatchObject({ message: 'Cannot remove all patches for addon "lower"' });
  expect((error as AggregateError).errors[0]).toMatchObject({ name: "Error", message: "rebuild refused" });
  expect(other.n).toBe(1); expect(lower.getPatches()).toHaveLength(1);
  expect(obj.value()).toBe("B(A(base))");
  refuse = false; lower.unpatchAll();
  expect(obj.value()).toBe("B(base)"); upper.unpatchAll();
});

test("malformed addon ids and failed patches do not create ownership", () => {
  for (const id of ["", "a".repeat(257), null, 4]) {
    fails(() => new PatchSet(id as string), "addon-registration", "PatchSet addon must contain 1..256 code units");
  }
  const set = new PatchSet("valid");
  fails(() => set.patch(Object.prototype, () => ({})), "patch-target", "Cannot patch a shared built-in prototype");
  fails(() => set.patch({ n: 1 }, null as never), "patch-target", "Patch requires a factory function");
  expect(set.getPatches()).toEqual([]);
});

/// (partial: patch rollback on registration failure)
test("registration failure rolls back patches and preserves its original error", () => {
  const obj = { n: 1 }; const cause = new Error("registration refused");
  let error: unknown;
  try { registerAddons([
    { id: "a", register(ctx) { ctx.patch(obj, () => ({ n: 2 })); } },
    { id: "b", register(ctx) { ctx.patch(obj, () => ({ n: 3 })); throw cause; } },
  ], { registry: new RegistryHub() }); } catch (e) { error = e; }
  expect(error).toBe(cause);
  expect(obj.n).toBe(1);
});

test("removed or failed addon contexts cannot install late patches", () => {
  const obj = { n: 1 }; const set = new PatchSet("closed"); set.unpatchAll();
  fails(() => set.patch(obj, () => ({ n: 2 })), "patch-target", 'PatchSet "closed" is closed');
  let saved: RegistrationContext | undefined;
  const cause = new Error("stop"); let error: unknown;
  try { registerAddons([{ id: "failed", register(ctx) { saved = ctx; throw cause; } }],
    { registry: new RegistryHub() }); } catch (e) { error = e; }
  expect(error).toBe(cause);
  fails(() => saved!.patch(obj, () => ({ n: 2 })), "patch-target", 'PatchSet "failed" is closed');
  expect(obj.n).toBe(1);
});

test("reentrant mutation of a set fails without leaking patches on another target", () => {
  const set = new PatchSet("reentry"); const obj = { n: 1 }; const other = { n: 2 };
  fails(() => set.patch(obj, () => { set.patch(other, () => ({ n: 3 })); return { n: 4 }; }),
    "patch-target", "Cannot mutate a PatchSet from its factory");
  expect(obj.n).toBe(1); expect(other.n).toBe(2); expect(set.getPatches()).toEqual([]);
});

test("reentry guard also covers a factory rebuilt by another addon", () => {
  const lower = new PatchSet("lower"); const upper = new PatchSet("upper");
  const obj = { n: 1 }; const other = { n: 2 }; let reenter = false;
  const undo = lower.patch(obj, () => ({ n: 3 }));
  upper.patch(obj, () => { if (reenter) upper.patch(other, () => ({ n: 4 })); return { n: 5 }; });
  reenter = true;
  fails(undo, "patch-target", "Cannot mutate a PatchSet from its factory");
  expect(other.n).toBe(2); expect(upper.getPatches()).toHaveLength(1);
  reenter = false; lower.unpatchAll(); upper.unpatchAll();
});

test("ownership admission is bounded and removal releases every target", () => {
  const set = new PatchSet("bounded"); const targets = Array.from({ length: 4096 }, () => ({ n: 1 }));
  for (const obj of targets) set.patch(obj, () => ({ n: 2 }));
  fails(() => set.patch({ n: 1 }, () => ({ n: 2 })), "patch-target", 'PatchSet "bounded" exceeds 4096 patches');
  set.unpatchAll(); expect(targets.every(obj => obj.n === 1)).toBe(true);
});

test("registration cleanup failure exposes original error and remaining sets for retry", () => {
  const obj = { n: 1 }; let refuse = false; const external = new PatchSet("external");
  const cause = new Error("register refused"); let error: unknown;
  try {
    registerAddons([{ id: "failed", register(ctx) {
      ctx.patch(obj, () => ({ n: 2 }));
      external.patch(obj, () => { if (refuse) throw new Error("cleanup refused"); return { n: 3 }; });
      refuse = true; throw cause;
    } }], { registry: new RegistryHub() });
  } catch (e) { error = e; }
  expect(error).toBeInstanceOf(AggregateError);
  expect(error).toBeInstanceOf(AddonRegistrationError);
  expect(error).toMatchObject({ cause, message: "Addon registration and patch cleanup failed" });
  const failed = error as AddonRegistrationError;
  expect(failed.errors[0]).toBe(cause); expect(failed.patchSets.get("failed")!.getPatches()).toHaveLength(1);
  refuse = false; failed.patchSets.get("failed")!.unpatchAll(); external.unpatchAll(); expect(obj.n).toBe(1);
});

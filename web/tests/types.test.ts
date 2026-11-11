/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The public API's types (review "Missing coverage", types): checked by tsc over tests/ (ci.sh web-tsc),
// which fails on a wrong expectation; the runtime half of each check is a no-op.

import { expectTypeOf, test } from "vitest";
import type * as Abi from "@erpforall/elang/abi";
import {
  Component,
  type ComponentClass,
  type ElangError,
  type Markup,
  type MountOptions,
  type PropsOf,
  type Row,
  type Signal,
  type Slot,
  ReactiveLoopError,
  UIError,
  type UIErrorCode,
  UnsafeAttributeError,
  computed,
  effect,
  isMarkup,
  markup,
  mount,
  onError,
  onMounted,
  onWillDestroy,
  onWillStart,
  onWillUnmount,
  signal,
  useEffect,
} from "@erpforall/elang";

class Typed extends Component<{ a: number }> {
  static override template = "web.Typed";
}

class Parent extends Component {
  static override template = "web.Parent";
  // A component with its own props type fits `static components`.
  static override components = { Typed };
}

/** Never called: the statements inside are checked by tsc only. */
function typeOnly(target: Element): void {
  const templates: never[] = [];
  void mount(Typed, target, { templates, props: { a: 1 } });
  // @ts-expect-error a prop of the wrong type
  void mount(Typed, target, { templates, props: { a: "1" } });
  // @ts-expect-error an unknown prop
  void mount(Typed, target, { templates, props: { a: 1, b: 2 } });
  // @ts-expect-error required root props left out (type review M3)
  void mount(Typed, target, { templates });
  // @ts-expect-error the options, and with them the templates, are required (type review L6)
  void mount(Parent, target);
  // @ts-expect-error templates are required (type review L6)
  void mount(Parent, target, {});
  void mount(Parent, target, { templates });
  onMounted(function () {
    expectTypeOf(this).toEqualTypeOf<Component<object>>();
  });
  // The hooks are generic in the component (type review L2): an annotated `this` keeps its props.
  onMounted(function (this: Typed) {
    expectTypeOf(this.props.a).toEqualTypeOf<number>();
  });
  // @ts-expect-error `this` must be a component
  onMounted(function (this: { x: 1 }) {});
  const s = signal(1);
  // @ts-expect-error a signal is written with set(), not by calling it with a value
  s(2);

  // Markup is nominal (type review H1): a string or a String object is not trusted HTML.
  // @ts-expect-error a plain string is not a Markup
  const m1: Markup = "<img src=x onerror=alert(1)>";
  // @ts-expect-error a String object is not a Markup
  const m2: Markup = new String("x");
  const m3: Markup = markup("<b>ok</b>");
  void [m1, m2, m3];
  const either = "x" as string | Markup;
  if (!isMarkup(either)) expectTypeOf(either).toEqualTypeOf<string>();

  // Async functions where only a synchronous one keeps its meaning (type review H2).
  class AsyncSetup extends Component {
    // @ts-expect-error setup() must be synchronous
    override async setup(): Promise<void> {}
  }
  void AsyncSetup;
  // @ts-expect-error an async onError handler
  onError(async () => {});
  // @ts-expect-error an async onMounted hook
  onMounted(async () => {});
  // @ts-expect-error an async onWillUnmount hook
  onWillUnmount(async () => {});
  // @ts-expect-error an async onWillDestroy hook
  onWillDestroy(async () => {});
  // @ts-expect-error an async effect
  effect(async () => {});
  // @ts-expect-error an async useEffect
  useEffect(async () => {});
  // @ts-expect-error an onWillStart returning a value that is not a promise
  onWillStart(() => 1);
  onWillStart(async () => {});
  onWillStart(() => {});
  effect(() => () => {}); // a cleanup is fine
  // @ts-expect-error an expression body that returns a value (write braces)
  effect(() => s());

  // Signal<T> is invariant and its setter is read-only (type review M1).
  // @ts-expect-error a Signal<string> is not a Signal<string | number>
  const wide: Signal<string | number> = signal("a");
  void wide;
  // @ts-expect-error the setter cannot be replaced
  s.set = () => {};
  // Type review L4, pinned: with a `T` that includes `void`, `set()` type-checks (and reads at run time).
  const v = signal<number | void>(1);
  v.set();

  // Props are read-only, also their keys (type review M2).
  class Writes extends Component<{ a: number }> {
    poke(): void {
      // @ts-expect-error props are read-only
      this.props.a = 2;
    }
  }
  void Writes;

  // UIError codes discriminate (type review L1).
  // @ts-expect-error the subclass codes are reserved for their classes
  void new UIError("x", "unsafe-attribute");
  const e = new UIError("x", "value") as ElangError;
  if (e.code === "unsafe-attribute") expectTypeOf(e).toEqualTypeOf<UnsafeAttributeError>();
  if (e.code === "reactive-loop") expectTypeOf(e).toEqualTypeOf<ReactiveLoopError>();

  // Slot.render is read-only (type review L3).
  const slot: Slot = { render: () => null };
  // @ts-expect-error a slot's render cannot be replaced
  slot.render = () => null;
}

test("the public API's types", () => {
  void typeOnly;
  expectTypeOf(Typed).toMatchTypeOf<ComponentClass>();
  // No module-private symbol in the public class type (type review L5: RENDER is on BuiltinClass only).
  expectTypeOf<Extract<keyof ComponentClass, symbol>>().toEqualTypeOf<never>();
  expectTypeOf(Parent.components.Typed).toEqualTypeOf<typeof Typed>();
  expectTypeOf<PropsOf<typeof Typed>>().toEqualTypeOf<{ a: number }>();
  expectTypeOf<PropsOf<ComponentClass>>().toEqualTypeOf<Record<string, unknown>>();
  expectTypeOf(mount<typeof Typed>).returns.toEqualTypeOf<Promise<Typed>>();
  expectTypeOf<MountOptions<{ a: number }>["props"]>().toEqualTypeOf<{ a: number }>();
  expectTypeOf<MountOptions<{ a?: number }>["props"]>().toEqualTypeOf<{ a?: number } | undefined>();

  expectTypeOf(signal(1)).toEqualTypeOf<Signal<number>>();
  expectTypeOf<Signal<number>>().parameters.toEqualTypeOf<[]>();
  expectTypeOf<Signal<number>["set"]>().parameters.toEqualTypeOf<[value: number]>();
  expectTypeOf(computed(() => "x")).toEqualTypeOf<() => string>();

  expectTypeOf<UIError["code"]>().toEqualTypeOf<UIErrorCode>();
  expectTypeOf<UnsafeAttributeError["code"]>().toEqualTypeOf<"unsafe-attribute">();
  expectTypeOf<UnsafeAttributeError["name"]>().toEqualTypeOf<"UnsafeAttributeError">();
  expectTypeOf<ReactiveLoopError["code"]>().toEqualTypeOf<"reactive-loop">();

  expectTypeOf(isMarkup).guards.toEqualTypeOf<Markup>();

  // The ABI types the public API re-exports are the frozen declarations of abi.d.ts.
  expectTypeOf<Row>().toEqualTypeOf<Abi.Row>();
  expectTypeOf<Slot>().toEqualTypeOf<Abi.Slot>();
  expectTypeOf<Row<number, string>["item"]>().returns.toEqualTypeOf<number>();
  expectTypeOf<Row<number, string>["value"]>().returns.toEqualTypeOf<string>();
  expectTypeOf<Row["item"]>().returns.toEqualTypeOf<unknown>();
  // abi.d.ts and the implementation agree exactly (abi-conformance.ts): `on` takes a void handler.
  expectTypeOf<typeof Abi.callHandler>().returns.toEqualTypeOf<void>();
  expectTypeOf<typeof Abi.on>().parameter(2).toEqualTypeOf<(ev: Event) => void>();
});

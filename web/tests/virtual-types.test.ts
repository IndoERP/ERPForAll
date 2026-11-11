/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expectTypeOf, test } from "vitest";
import { Virtual, type ComponentClass, type VirtualRow, mount, onWillUpdateProps } from "../src/index.js";

class NumberWindow extends Virtual<number> {}

function typeOnly(target: Element): void {
  const options = {templates: [], props: {
    items: [1, 2], getKey: (item: number) => item, itemHeight: 24, viewportHeight: 240,
  }};
  void mount(NumberWindow, target, options);
  // @ts-expect-error item type must agree with the specialized component
  void mount(NumberWindow, target, {templates: [], props: {...options.props, items: ["wrong"]}});
  // @ts-expect-error geometry is numeric
  void mount(NumberWindow, target, {templates: [], props: {...options.props, itemHeight: "24"}});
  onWillUpdateProps<{value: number}>(async next => { expectTypeOf(next.value).toEqualTypeOf<number>(); });
  const row: VirtualRow<number> = {item: () => 1, index: () => 0};
  // @ts-expect-error accessors cannot be replaced by slot code
  row.item = () => 2;
}

test("virtualizer and async hook public types", () => {
  void typeOnly;
  expectTypeOf(NumberWindow).toMatchTypeOf<ComponentClass>();
  expectTypeOf<VirtualRow<number>["item"]>().returns.toEqualTypeOf<number>();
  expectTypeOf<VirtualRow<number>["index"]>().returns.toEqualTypeOf<number>();
});

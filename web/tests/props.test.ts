/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Tests for component props schema declaration, scalar/composite validation, default factories,
// and strict universal validation across all execution modes.

import { describe, expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  Component,
  type TypeDef,
  UIError,
  type ValidationContext,
  flushSync,
  mount,
  onError,
  props,
  signal,
  types,
  unmount,
  validateAndApplyPropsDefaults,
} from "../src/index.js";
import { mountApp, tpl } from "./harness.js";

/** The error `fn` throws (the test fails when it throws nothing). */
function thrown(fn: () => unknown): unknown {
  try {
    fn();
  } catch (e) {
    return e;
  }
  throw new Error("expected an error, nothing was thrown");
}

const dummyTemplate = {
  name: "test.Tpl",
  render: () => document.createElement("div"),
};

describe("Component props schema validation", () => {
  test(": props schema declaration with scalar type validation", async () => {
    class ScalarComp extends Component<{
      title: string;
      age: number;
      active: boolean;
    }> {
      static override template = "test.Tpl";
      static props = props({
        title: types.string,
        age: types.number,
        active: types.boolean,
      });
    }

    const container = document.createElement("div");

    // Passing valid scalar props succeeds
    const root = await mount(ScalarComp, container, {
      templates: [dummyTemplate],
      props: { title: "Alice", age: 30, active: true },
    });
    expect(root).toBeDefined();
    unmount(container);

    // Passing invalid scalar prop (string for age) rejects mount promise loudly
    const container2 = document.createElement("div");
    await expect(
      mount(ScalarComp, container2, {
        templates: [dummyTemplate],
        props: { title: "Alice", age: ("thirty" as unknown as number), active: true },
      }),
    ).rejects.toThrowError(/(?=.*ScalarComp)(?=.*props)(?=.*age)(?=.*number)/is);
  });

  test(": optional props with default factory fallback", async () => {
    class DefaultComp extends Component<{
      req: string;
      opt?: number;
      def?: string;
    }> {
      static override template = "test.Tpl";
      static props = props({
        req: types.string,
        opt: types.optional(types.number),
        def: types.optional(types.string, () => "default_val"),
      });
    }

    const container = document.createElement("div");
    const instance = await mount(DefaultComp, container, {
      templates: [dummyTemplate],
      props: { req: "hello" },
    });

    expect(instance.props.req).toBe("hello");
    expect(instance.props.opt).toBeUndefined();
    expect(instance.props.def).toBe("default_val");

    // Passing def as undefined explicitly also receives default factory value
    const container2 = document.createElement("div");
    const instance2 = await mount(DefaultComp, container2, {
      templates: [dummyTemplate],
      props: { req: "world", def: undefined },
    });
    expect(instance2.props.req).toBe("world");
    expect(instance2.props.def).toBe("default_val");
    unmount(container2);

    unmount(container);
  });

  test(": complex props validation with composite schema types (array, object, strictObject, or, record)", async () => {
    class ComplexComp extends Component<{
      tags: string[];
      meta: { id: number; flag?: boolean };
      exact: { key: string };
      choice: string | number;
    }> {
      static override template = "test.Tpl";
      static props = props({
        tags: types.array(types.string),
        meta: types.object({
          id: types.number,
          flag: types.optional(types.boolean),
        }),
        exact: types.strictObject({
          key: types.string,
        }),
        choice: types.or(types.string, types.number),
      });
    }

    const container = document.createElement("div");

    // Valid composite props succeed
    const root = await mount(ComplexComp, container, {
      templates: [dummyTemplate],
      props: {
        tags: ["a", "b"],
        meta: { id: 1 },
        exact: { key: "exactKey" },
        choice: 123,
      },
    });
    expect(root).toBeDefined();
    unmount(container);

    // strictObject fails on unknown keys
    const container2 = document.createElement("div");
    await expect(
      mount(ComplexComp, container2, {
        templates: [dummyTemplate],
        props: {
          tags: ["a"],
          meta: { id: 1 },
          exact: { key: "k", unknownKey: 999 } as any,
          choice: "str",
        },
      }),
    ).rejects.toThrowError(/unknown keys/i);

    // array validation traverses elements
    const container3 = document.createElement("div");
    await expect(
      mount(ComplexComp, container3, {
        templates: [dummyTemplate],
        props: {
          tags: [123 as any],
          meta: { id: 1 },
          exact: { key: "k" },
          choice: "str",
        },
      }),
    ).rejects.toThrowError(/tags.*string/i);
  });

  test(": props validation timing and strict universal mode validation", async () => {
    class StrictComp extends Component<{ code: number }> {
      static override template = "test.Tpl";
      static props = props({
        code: types.number,
      });
    }

    const container = document.createElement("div");

    // Regardless of dev/prod flags, ERPForAll fails loudly on invalid props
    await expect(
      mount(StrictComp, container, {
        templates: [dummyTemplate],
        props: { code: ("invalid" as unknown as number) },
      }),
    ).rejects.toMatchObject({
      code: "value",
      message: "Invalid props passed to component StrictComp:\n  - at code: expected number, received string",
    });
  });

  test(": an optional prop without a default that starts undefined still follows its binding (R1)", async () => {
    const opt = signal<number | undefined>(undefined);
    const def = signal<string | undefined>(undefined);
    class Follow extends Component<{ opt?: number; def?: string }> {
      static override template = "test.Tpl";
      static props = props({
        opt: types.optional(types.number),
        def: types.optional(types.string, () => "fallback"),
      });
    }
    const { instance } = await mountApp(Follow, [dummyTemplate], {
      props: {
        get opt() {
          return opt();
        },
        get def() {
          return def();
        },
      },
    });
    expect(instance.props.opt).toBeUndefined();
    expect(instance.props.def).toBe("fallback");
    opt.set(5);
    def.set("given");
    flushSync();
    expect(instance.props.opt).toBe(5);
    expect(instance.props.def).toBe("given");
    def.set(undefined);
    flushSync();
    expect(instance.props.def).toBe("fallback");
  });

  test(": a prop updated to an invalid value fails loudly and the child keeps its valid props (R7)", async () => {
    const code = signal<unknown>(1);
    const caught: unknown[] = [];
    let child!: Component<{ code: number }>;
    class Checked extends Component<{ code: number }> {
      static override template = "test.Tpl";
      static props = props({ code: types.number });
      override setup() {
        child = this;
      }
    }
    class Host extends Component {
      static override template = "test.CheckedHost";
      static override components = { Checked };
      override setup() {
        onError((e) => void caught.push(e));
      }
    }
    await mountApp(Host, [
      dummyTemplate,
      tpl("test.CheckedHost", ($self) => {
        const n = document.createElement("div");
        $.component(n, null, $self, "Checked", { code: () => code() }, null, null);
        return n;
      }),
    ]);
    code.set("seven");
    flushSync();
    expect(caught).toHaveLength(1);
    expect(caught[0]).toBeInstanceOf(UIError);
    expect((caught[0] as UIError).code).toBe("value");
    expect((caught[0] as UIError).message).toBe(
      "Invalid props passed to component Checked:\n  - at code: expected number, received string",
    );
    expect(child.props.code).toBe(1);

    code.set(undefined);
    flushSync();
    expect(caught).toHaveLength(2);
    expect((caught[1] as UIError).message).toBe(
      'Invalid props passed to component Checked:\n  - missing required prop "code"',
    );
    expect(child.props.code).toBe(1);

    code.set(2);
    flushSync();
    expect(caught).toHaveLength(2);
    expect(child.props.code).toBe(2);
  });

  test("literal() reports a cyclic or BigInt value as a UIError, not a TypeError (R16)", () => {
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    const schema = props({ x: types.literal("a") });
    const e1 = thrown(() => validateAndApplyPropsDefaults("Lit", { x: cyclic }, schema));
    expect(e1).toBeInstanceOf(UIError);
    expect((e1 as UIError).code).toBe("value");
    expect((e1 as UIError).message).toBe(
      'Invalid props passed to component Lit:\n  - at x: expected literal "a", received [object Object]',
    );
    const e2 = thrown(() => validateAndApplyPropsDefaults("Lit", { x: 10n }, schema));
    expect(e2).toBeInstanceOf(UIError);
    expect((e2 as UIError).message).toBe('Invalid props passed to component Lit:\n  - at x: expected literal "a", received 10n');
    const e3 = thrown(() => validateAndApplyPropsDefaults("Lit", { x: 1n }, props({ x: types.literal(2n) })));
    expect((e3 as UIError).message).toBe("Invalid props passed to component Lit:\n  - at x: expected literal 2n, received 1n");
  });

  test("validators report null and arrays by name, not as object (R19)", () => {
    const e = thrown(() =>
      validateAndApplyPropsDefaults("Kinds", { s: null, o: [1] }, props({ s: types.string, o: types.object({}) })),
    );
    expect((e as UIError).message).toBe(
      "Invalid props passed to component Kinds:\n  - at s: expected string, received null\n  - at o: expected object, received array",
    );
  });

  test("TypeDef and schemas are immutable, props() keeps the shape type and ValidationContext is public (R19)", () => {
    const schema = props({ a: types.string, b: types.optional(types.number, () => 1) });
    // The shape keeps its keys: `schema.shape.a` is a TypeDef, not `TypeDef | undefined`.
    const a: TypeDef = schema.shape.a;
    expect(a).toBe(types.string);
    expect(schema.shape.b.isOptional).toBe(true);
    expect(() => {
      (types.string as unknown as { isOptional: boolean }).isOptional = true;
    }).toThrow(TypeError);
    expect(() => {
      (schema.shape as Record<string, TypeDef>).c = types.any;
    }).toThrow(TypeError);
    const issues: string[] = [];
    const ctx: ValidationContext = {
      value: 3,
      path: ["n"],
      addIssue: (m) => void issues.push(m),
      withKey: () => ctx,
    };
    types.string(ctx);
    expect(issues).toEqual(["expected string, received number"]);
  });
});

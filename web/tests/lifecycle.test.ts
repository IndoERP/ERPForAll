/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Tests for component lifecycle hooks (onFlushed, onWillUpdateProps, parent-first flush order).

import { describe, expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  Component,
  type Slot,
  effect,
  flushSync,
  mount,
  onError,
  onFlushed,
  onWillUpdateProps,
  props,
  signal,
  types,
  unmount,
} from "../src/index.js";
import { mountApp, settle, tpl } from "./harness.js";

describe("Lifecycle hooks and flush order", () => {
  /// (partial: synchronous execution prior to prop signal updates; held async behavior covered in async-props.test.ts)
  test(" (partial: synchronous execution prior to prop signal updates): onWillUpdateProps runs before prop signals update", async () => {
    let capturedCurrent: number | undefined;
    let capturedNext: number | undefined;
    let hookRuns = 0;

    class Child extends Component<{ val: number }> {
      static override template = "test.Child";
      override setup() {
        onWillUpdateProps<{ val: number }>((nextProps) => {
          hookRuns++;
          capturedCurrent = this.props.val;
          capturedNext = nextProps.val;
        });
      }
    }

    const parentVal = signal(1);

    const templates = [
      {
        name: "test.Child",
        render: () => document.createElement("div"),
      },
    ];

    const container = document.createElement("div");
    const instance = await mount(Child, container, {
      templates,
      props: {
        get val() {
          return parentVal();
        },
      },
    });

    expect(hookRuns).toBe(0); // Not called on initial mount

    // Update parent signal
    parentVal.set(2);
    flushSync();

    expect(hookRuns).toBe(1);
    expect(capturedCurrent).toBe(1);
    expect(capturedNext).toBe(2);

    unmount(container);
  });

  /// (partial: batched multi-prop updates trigger onWillUpdateProps once with complete snapshot)
  test(" (partial: batched multi-prop updates trigger onWillUpdateProps once with complete snapshot): onWillUpdateProps fires once for multi-prop changes", async () => {
    let hookRuns = 0;
    let lastSnapshot: { a: number; b: string } | undefined;
    let oldA: number | undefined;
    let oldB: string | undefined;

    class MultiChild extends Component<{ a: number; b: string }> {
      static override template = "test.MultiChild";
      override setup() {
        onWillUpdateProps<{ a: number; b: string }>((nextProps) => {
          hookRuns++;
          lastSnapshot = { ...nextProps };
          oldA = this.props.a;
          oldB = this.props.b;
        });
      }
    }

    const sigA = signal(1);
    const sigB = signal("one");

    const templates = [
      {
        name: "test.MultiChild",
        render: () => document.createElement("div"),
      },
    ];

    const container = document.createElement("div");
    await mount(MultiChild, container, {
      templates,
      props: {
        get a() {
          return sigA();
        },
        get b() {
          return sigB();
        },
      },
    });

    expect(hookRuns).toBe(0);

    // Update both signals simultaneously
    sigA.set(2);
    sigB.set("two");
    flushSync();

    // Hook must run exactly ONCE, observing old props on this.props and both new props in nextProps
    expect(hookRuns).toBe(1);
    expect(oldA).toBe(1);
    expect(oldB).toBe("one");
    expect(lastSnapshot).toEqual({ a: 2, b: "two" });

    unmount(container);
  });

  /// (partial: runtime half, onFlushed hook execution after DOM mutations flush)
  test(" (partial: runtime half, onFlushed hook execution after DOM mutations flush): onFlushed executes post-flush", async () => {
    let flushedCount = 0;
    const textSignal = signal("hello");
    let domText = "";
    let domNode!: HTMLDivElement;

    class FlushedComp extends Component {
      static override template = "test.FlushedComp";
      override setup() {
        onFlushed(() => {
          flushedCount++;
          domText = domNode.textContent ?? "";
        });
      }
    }

    const templates = [
      {
        name: "test.FlushedComp",
        render: () => {
          domNode = document.createElement("div");
          effect(() => {
            domNode.textContent = textSignal();
          });
          return domNode;
        },
      },
    ];

    const container = document.createElement("div");
    await mount(FlushedComp, container, { templates });
    flushSync();

    // The first render is a mount, not a patch: onMounted covers it (review round 3, R8).
    expect(flushedCount).toBe(0);

    // Signal change triggers effect and post-flush hook
    textSignal.set("world");
    flushSync();

    expect(flushedCount).toBe(1);
    expect(domText).toBe("world");

    // Unrelated signal change in the app does not trigger this component's onFlushed hook
    const unrelatedSignal = signal(100);
    effect(() => {
      unrelatedSignal();
    });
    flushSync();
    expect(flushedCount).toBe(1);

    unrelatedSignal.set(200);
    flushSync();
    expect(flushedCount).toBe(1);

    unmount(container);
  });

  test("Parent-first flush order prevents child effects from running against torn tree", () => {
    const showChild = signal(true);
    const childVal = signal(10);
    let childRuns = 0;

    const parentOwner = { depth: 1, disposed: false };
    const childOwner = { depth: 2, disposed: false };

    // When showChild becomes false, parent tears down child
    effect(() => {
      if (!showChild()) {
        childOwner.disposed = true;
      }
    });

    effect(() => {
      childVal();
      if (!childOwner.disposed) {
        childRuns++;
      }
    });

    flushSync();
    expect(childRuns).toBe(1);

    // Concurrently hide child and change child value
    showChild.set(false);
    childVal.set(20);
    flushSync();

    // Child effect did NOT run because parent disposed child first in tree order
    expect(childRuns).toBe(1);
  });
});

describe("Review round 3: onFlushed", () => {
  test("a signal written inside onFlushed is applied in the same flush (R2)", async () => {
    const text = signal("a");
    const echo = signal("");
    let node!: HTMLElement;
    class Writer extends Component {
      static override template = "test.Writer";
      override setup() {
        onFlushed(() => {
          if (echo() !== text()) echo.set(text());
        });
      }
    }
    await mountApp(Writer, [
      tpl("test.Writer", () => {
        node = document.createElement("p");
        effect(() => {
          node.textContent = `${text()}|${echo()}`;
        });
        return node;
      }),
    ]);
    text.set("b");
    flushSync();
    expect(node.textContent).toBe("b|b");
  });

  test("a component's mount does not make an unrelated later flush run its onFlushed (R8)", async () => {
    let flushed = 0;
    class Quiet extends Component {
      static override template = "test.Quiet";
      override setup() {
        onFlushed(() => void flushed++);
      }
    }
    await mountApp(Quiet, [tpl("test.Quiet", () => document.createElement("p"))]);
    const unrelated = signal(0);
    const stop = effect(() => {
      unrelated();
    });
    unrelated.set(1);
    flushSync();
    stop();
    expect(flushed).toBe(0);
  });

  test("a rejected promise returned by onFlushed reaches the component's onError (R18)", async () => {
    const caught: unknown[] = [];
    const failure = new Error("flushed failed");
    const text = signal("a");
    class Async extends Component {
      static override template = "test.Async";
      override setup() {
        onError((e) => void caught.push(e));
        onFlushed((() => Promise.reject(failure)) as unknown as () => void);
      }
    }
    await mountApp(Async, [
      tpl("test.Async", () => {
        const p = document.createElement("p");
        effect(() => {
          p.textContent = text();
        });
        return p;
      }),
    ]);
    text.set("b");
    flushSync();
    await settle();
    expect(caught).toEqual([failure]);
  });
});

describe("Review round 3: onWillUpdateProps", () => {
  test("signals the hook reads do not become dependencies of the prop bindings (R5)", async () => {
    const val = signal(1);
    const unrelated = signal(0);
    let thunkCalls = 0;
    let hookRuns = 0;
    class Reader extends Component<{ val: number }> {
      static override template = "test.Reader";
      override setup() {
        onWillUpdateProps(() => {
          hookRuns++;
          unrelated();
        });
      }
    }
    await mountApp(Reader, [tpl("test.Reader", () => document.createElement("p"))], {
      props: {
        get val() {
          thunkCalls++;
          return val();
        },
      },
    });
    val.set(2);
    flushSync();
    const callsAfterUpdate = thunkCalls;
    expect(hookRuns).toBe(1);
    unrelated.set(1);
    flushSync();
    expect(thunkCalls).toBe(callsAfterUpdate);
    expect(hookRuns).toBe(1);
  });

  test("a t-props spread change runs onWillUpdateProps, and a removed spread key falls back to its schema default (R6)", async () => {
    const spread = signal<Record<string, unknown>>({ size: "large" });
    const seen: Array<Record<string, unknown>> = [];
    let child!: Component<{ size: string }>;
    class SpreadChild extends Component<{ size: string }> {
      static override template = "test.SpreadChild";
      static props = props({ size: types.optional(types.string, () => "medium") });
      override setup() {
        child = this;
        onWillUpdateProps((next) => void seen.push({ ...next }));
      }
    }
    class Host extends Component {
      static override template = "test.Host";
      static override components = { SpreadChild };
    }
    await mountApp(Host, [
      tpl("test.Host", ($self) => {
        const n = document.createElement("div");
        $.component(n, null, $self, "SpreadChild", {}, () => spread(), null);
        return n;
      }),
      tpl("test.SpreadChild", () => document.createElement("p")),
    ]);
    expect(child.props.size).toBe("large");
    spread.set({ size: "small" });
    flushSync();
    expect(seen).toEqual([{ size: "small" }]);
    expect(child.props.size).toBe("small");
    spread.set({});
    flushSync();
    expect(seen).toEqual([{ size: "small" }, { size: "medium" }]);
    expect(child.props.size).toBe("medium");
  });

  test("nextProps carries the slots and the props that only have a default (R20)", async () => {
    const label = signal("one");
    let next: Record<string, unknown> | undefined;
    const slots: Record<string, Slot> = { default: { render: () => document.createTextNode("x") } };
    class Slotted extends Component<{ label: string; tone?: string; slots?: Record<string, Slot> }> {
      static override template = "test.Slotted";
      static props = props({
        label: types.string,
        tone: types.optional(types.string, () => "plain"),
        slots: types.optional(types.object({})),
      });
      override setup() {
        onWillUpdateProps((n) => void (next = { ...n }));
      }
    }
    class Host extends Component {
      static override template = "test.SlotHost";
      static override components = { Slotted };
    }
    await mountApp(Host, [
      tpl("test.SlotHost", ($self) => {
        const n = document.createElement("div");
        $.component(n, null, $self, "Slotted", { label: () => label() }, null, slots);
        return n;
      }),
      tpl("test.Slotted", () => document.createElement("p")),
    ]);
    label.set("two");
    flushSync();
    expect(next).toEqual({ label: "two", tone: "plain", slots });
  });

  test("an onWillUpdateProps promise is accepted and delays prop writes", async () => {
    const caught: unknown[] = [];
    const val = signal(1);
    class Held extends Component<{ val: number }> {
      static override template = "test.Held";
      override setup() {
        onWillUpdateProps(() => Promise.resolve());
      }
    }
    class Host extends Component {
      static override template = "test.HeldHost";
      static override components = { Held };
      override setup() {
        onError((e) => void caught.push(e));
      }
    }
    await mountApp(Host, [
      tpl("test.HeldHost", ($self) => {
        const n = document.createElement("div");
        $.component(n, null, $self, "Held", { val: () => val() }, null, null);
        return n;
      }),
      tpl("test.Held", () => document.createElement("p")),
    ]);
    val.set(2);
    flushSync();
    await settle();
    expect(caught).toEqual([]);
  });
});

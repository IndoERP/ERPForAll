/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
import { describe, expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { Component, effect, flushSync, onError, onWillUpdateProps, props, signal, types, unmount } from "../src/index.js";
import { mountApp, settle, tpl } from "./harness.js";

function deferred() {
  let resolve!: () => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

async function fixture() {
  const value = signal(0);
  const rounds: Array<{ value: number; hold: ReturnType<typeof deferred> }> = [];
  class Child extends Component<{ value: number }> {
    static override template = "async.Child";
    override setup() {
      onWillUpdateProps<{ value: number }>((next) => {
        const hold = deferred();
        rounds.push({ value: next.value, hold });
        return hold.promise;
      });
    }
  }
  const errors: unknown[] = [];
  const result = await mountApp(Child, [tpl("async.Child", self => {
    const node = document.createElement("div");
    effect(() => { node.textContent = String(self.props["value"]); });
    return node;
  })], {
    props: { get value() { return value(); } }, errors,
  });
  return { ...result, value, rounds, errors };
}

describe("held async prop commits", () => {
  test("holds current props until resolution", async () => {
    const f = await fixture();
    f.value.set(1); flushSync();
    expect(f.rounds.map(r => r.value)).toEqual([1]);
    expect(f.instance.props.value).toBe(0);
    expect(f.root.textContent).toBe("0");
    f.rounds[0]!.hold.resolve(); await settle();
    expect(f.instance.props.value).toBe(1);
    expect(f.root.textContent).toBe("1");
    expect(f.errors).toEqual([]);
  });

  test("newer candidate wins regardless of resolution order", async () => {
    const f = await fixture();
    f.value.set(1); flushSync();
    f.value.set(2); flushSync();
    f.rounds[1]!.hold.resolve(); await settle();
    expect(f.instance.props.value).toBe(2);
    f.rounds[0]!.hold.resolve(); await settle();
    expect(f.instance.props.value).toBe(2);
    expect(f.errors).toEqual([]);
  });

  test("reversion to committed props cancels a held candidate", async () => {
    const f = await fixture();
    f.value.set(1); flushSync();
    f.value.set(0); flushSync();
    f.rounds[0]!.hold.resolve(); await settle();
    expect(f.instance.props.value).toBe(0);
    expect(f.errors).toEqual([]);
  });

  test("unmount prevents a late commit", async () => {
    const f = await fixture();
    f.value.set(1); flushSync();
    unmount(f.root);
    f.rounds[0]!.hold.resolve(); await settle();
    expect(f.instance.props.value).toBe(0);
    expect(f.errors).toEqual([]);
  });

  test("rejection reports the original failure without applying props", async () => {
    const f = await fixture();
    f.value.set(1); flushSync();
    const failure = new Error("async preparation failed");
    f.rounds[0]!.hold.reject(failure); await settle();
    expect(f.instance.props.value).toBe(0);
    expect(f.errors).toHaveLength(1);
    expect(f.errors[0]).toBe(failure);
    expect(f.errors[0]).toMatchObject({name: "Error", message: "async preparation failed"});
  });
});

/// (partial: parallel async hooks and complete-snapshot deduplication; hold and synchronous cases elsewhere)
test("starts every hook in parallel and deduplicates a simultaneous multi-prop snapshot", async () => {
  const a = signal(0), b = signal(0);
  const first = deferred(), second = deferred();
  const seen: object[] = [];
  class Child extends Component<{ a: number; b: number }> {
    static override template = "async.Multi";
    override setup() {
      onWillUpdateProps<{ a: number; b: number }>(next => { seen.push(next); return first.promise; });
      onWillUpdateProps<{ a: number; b: number }>(next => { seen.push(next); return second.promise; });
    }
  }
  const { instance } = await mountApp(Child, [tpl("async.Multi", () => document.createElement("div"))], {
    props: { get a() { return a(); }, get b() { return b(); } },
  });
  a.set(1); b.set(2); flushSync();
  expect(seen).toEqual([{ a: 1, b: 2 }, { a: 1, b: 2 }]);
  expect(instance.props.a).toBe(0);
  first.resolve(); await settle(); expect(instance.props.a).toBe(0);
  second.resolve(); await settle();
  expect({ ...instance.props }).toEqual({ a: 1, b: 2 });
});

test("a synchronous hook failure refuses the candidate and still observes other hook rejections", async () => {
  const value = signal(0), hold = deferred();
  const failure = new Error("synchronous preparation failed");
  const later = new Error("parallel preparation failed");
  const caught: unknown[] = [];
  class Child extends Component<{ value: number }> {
    static override template = "async.Failed";
    override setup() {
      onError(e => { caught.push(e); });
      onWillUpdateProps(() => { throw failure; });
      onWillUpdateProps(() => hold.promise);
    }
  }
  const { instance } = await mountApp(Child, [tpl("async.Failed", () => document.createElement("div"))], {
    props: { get value() { return value(); } }, errors: caught,
  });
  value.set(1); flushSync();
  expect(instance.props.value).toBe(0);
  expect(caught[0]).toBe(failure);
  expect(caught[0]).toMatchObject({name: "Error", message: "synchronous preparation failed"});
  hold.reject(later); await settle();
  expect(caught[1]).toBe(later);
  expect(caught[1]).toMatchObject({name: "Error", message: "parallel preparation failed"});
});

test("a failing synchronous hook is not repeated by sibling bindings of the same snapshot", async () => {
  const a = signal(0), b = signal(0);
  const failure = new Error("candidate refused once");
  const caught: unknown[] = [];
  class Child extends Component<{a: number; b: number}> {
    static override template = "async.Once";
    override setup() {
      onError(e => { caught.push(e); });
      onWillUpdateProps(() => { throw failure; });
    }
  }
  const {instance} = await mountApp(Child, [tpl("async.Once", () => document.createElement("div"))], {
    props: {get a() {return a();}, get b() {return b();}},
  });
  a.set(1); b.set(2); flushSync();
  expect(caught).toHaveLength(1);
  expect(caught[0]).toBe(failure);
  expect(caught[0]).toMatchObject({name: "Error", message: "candidate refused once"});
  expect({...instance.props}).toEqual({a: 0, b: 0});
});

test("a superseded rejection is reported without destroying the newer successful component", async () => {
  const f = await fixture();
  f.value.set(1); flushSync(); f.value.set(2); flushSync();
  f.rounds[1]!.hold.resolve(); await settle();
  const stale = new Error("obsolete preparation failed");
  f.rounds[0]!.hold.reject(stale); await settle();
  expect(f.errors[0]).toBe(stale);
  expect(f.errors[0]).toMatchObject({name: "Error", message: "obsolete preparation failed"});
  expect(f.instance.props.value).toBe(2);
  f.value.set(3); flushSync();
  expect(f.rounds.map(r => r.value)).toEqual([1, 2, 3]);
  f.rounds[2]!.hold.resolve(); await settle();
  expect(f.instance.props.value).toBe(3);
});

test("an invalid newer candidate cancels held props while its parent handles validation and later recovers", async () => {
  const value = signal<number | string>(0);
  const caught: unknown[] = [];
  const holds: ReturnType<typeof deferred>[] = [];
  let child!: Checked;
  class Checked extends Component<{value: number}> {
    static override template = "async.Checked";
    static props = props({value: types.number});
    override setup() {
      child = this;
      onWillUpdateProps(() => {const hold = deferred(); holds.push(hold); return hold.promise;});
    }
  }
  class Host extends Component {
    static override template = "async.CheckedHost";
    static override components = {Checked};
    override setup() {onError(error => {caught.push(error);});}
  }
  const {root} = await mountApp(Host, [
    tpl("async.CheckedHost", self => {
      const node = document.createElement("div");
      $.component(node, null, self, "Checked", {value: () => value()}, null, null);
      return node;
    }),
    tpl("async.Checked", self => {
      const node = document.createElement("span");
      $.effect(() => {node.textContent = String(self.props["value"]);});
      return node;
    }),
  ]);
  value.set(1); flushSync();
  value.set("invalid"); flushSync();
  expect(caught).toHaveLength(1);
  expect(caught[0]).toMatchObject({name: "UIError", code: "value", message: "Invalid props passed to component Checked:\n  - at value: expected number, received string"});
  holds[0]!.resolve(); await settle();
  expect(child.props.value).toBe(0); expect(root.textContent).toBe("0");
  value.set(2); flushSync(); holds[1]!.resolve(); await settle();
  expect(child.props.value).toBe(2); expect(root.textContent).toBe("2");
});

test("an invalid spread cancels a held update before its parent catches the error", async () => {
  const spread = signal<unknown>({value: 0});
  const caught: unknown[] = [], holds: ReturnType<typeof deferred>[] = [];
  let child!: SpreadChild;
  class SpreadChild extends Component<{value: number}> {
    static override template = "async.InvalidSpreadChild";
    override setup() {
      child = this;
      onWillUpdateProps(() => {const hold = deferred(); holds.push(hold); return hold.promise;});
    }
  }
  class Host extends Component {
    static override template = "async.InvalidSpreadHost";
    static override components = {SpreadChild};
    override setup() {onError(error => {caught.push(error);});}
  }
  await mountApp(Host, [
    tpl("async.InvalidSpreadHost", self => {
      const node = document.createElement("div");
      $.component(node, null, self, "SpreadChild", {}, () => spread() as object, null);
      return node;
    }), tpl("async.InvalidSpreadChild", () => document.createElement("span")),
  ]);
  spread.set({value: 1}); flushSync();
  spread.set("invalid"); flushSync();
  expect(caught).toHaveLength(1);
  expect(caught[0]).toMatchObject({name: "UIError", code: "value", message: "t-props takes an object, got string"});
  holds[0]!.resolve(); await settle();
  expect(child.props.value).toBe(0);
  spread.set({value: 2}); flushSync(); holds[1]!.resolve(); await settle();
  expect(child.props.value).toBe(2);
});

test("held spread updates use defaults and discard obsolete added values", async () => {
  const spread = signal<{value?: number}>({value: 0});
  const holds: ReturnType<typeof deferred>[] = [], seen: number[] = [];
  let child!: Defaulted;
  class Defaulted extends Component<{value: number}> {
    static override template = "async.Defaulted";
    static props = props({value: types.optional(types.number, () => 9)});
    override setup() {
      child = this;
      onWillUpdateProps<{value: number}>(next => {
        seen.push(next.value); const hold = deferred(); holds.push(hold); return hold.promise;
      });
    }
  }
  class Host extends Component {
    static override template = "async.DefaultedHost";
    static override components = {Defaulted};
  }
  await mountApp(Host, [
    tpl("async.DefaultedHost", self => {
      const node = document.createElement("div");
      $.component(node, null, self, "Defaulted", {}, () => spread(), null);
      return node;
    }), tpl("async.Defaulted", () => document.createElement("span")),
  ]);
  spread.set({value: 1}); flushSync();
  spread.set({}); flushSync();
  holds[0]!.resolve(); await settle(); expect(child.props.value).toBe(0);
  holds[1]!.resolve(); await settle(); expect(child.props.value).toBe(9);
  spread.set({value: 2}); flushSync(); spread.set({}); flushSync();
  holds[2]!.resolve(); await settle(); expect(child.props.value).toBe(9);
  expect(seen).toEqual([1, 9, 2]);
});


test.each(["explicit thunk", "spread expression", "spread property", "sibling snapshot"] as const)(
  "a %s evaluation failure cancels an older hold and preserves the original error",
  async source => {
    const value = signal(0), failNow = signal(false);
    let failSibling = false;
    const failure = new Error("prop evaluation failed");
    const caught: unknown[] = [], holds: ReturnType<typeof deferred>[] = [];
    let child!: Evaluated;
    class Evaluated extends Component<{value: number; other?: number}> {
      static override template = "async.Evaluated";
      override setup() {
        child = this;
        onWillUpdateProps(() => {const hold = deferred(); holds.push(hold); return hold.promise;});
      }
    }
    class Host extends Component {
      static override template = "async.EvaluatedHost";
      static override components = {Evaluated};
      override setup() {onError(error => {caught.push(error);});}
    }
    await mountApp(Host, [
      tpl("async.EvaluatedHost", self => {
        const node = document.createElement("div");
        const thunks: Record<string, () => unknown> = source === "explicit thunk" ? {value: () => {
          const next = value();
          if (failNow()) throw failure;
          return next;
        }} : source === "sibling snapshot" ? {value: () => value(), other: () => {
          if (failSibling) throw failure;
          return 0;
        }} : {};
        const spread = source === "spread expression" || source === "spread property" ? () => {
          const next = value(), failing = failNow();
          if (source === "spread expression" && failing) throw failure;
          return {get value() {if (failing) throw failure; return next;}};
        } : null;
        $.component(node, null, self, "Evaluated", thunks, spread, null);
        return node;
      }), tpl("async.Evaluated", () => document.createElement("span")),
    ]);
    value.set(1); flushSync();
    expect(holds).toHaveLength(1);
    if (source === "sibling snapshot") {failSibling = true; value.set(2);}
    else failNow.set(true);
    flushSync();
    expect(caught).toHaveLength(1);
    expect(caught[0]).toBe(failure);
    expect(caught[0]).toMatchObject({name: "Error", message: "prop evaluation failed"});
    holds[0]!.resolve(); await settle();
    expect(child.props.value).toBe(0);
    failSibling = false; failNow.set(false);
    const recovered = source === "sibling snapshot" ? 3 : 2;
    value.set(recovered); flushSync();
    expect(holds).toHaveLength(2);
    holds[1]!.resolve(); await settle();
    expect(child.props.value).toBe(recovered);
    expect(caught).toHaveLength(1);
  },
);

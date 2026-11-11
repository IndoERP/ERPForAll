/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Event helpers: on, callHandler, delegation.

import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { UIError, flushSync, onError, signal } from "../src/index.js";
import { CAPTURE, PASSIVE, PREVENT, SELF, STOP, SYNTHETIC } from "../src/runtime/events.js";
import { comp, mountApp, settle } from "./harness.js";

function click(el: Element): MouseEvent {
  const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
  el.dispatchEvent(ev);
  return ev;
}

/// (partial: runtime half, the stop, prevent, self, capture and synthetic modifiers)
test("on applies stop, prevent and self, and synthetic handlers are delegated to the root", async () => {
  const log: string[] = [];
  const { C, t } = comp("T", ($self) => {
    const n = ($.template('<div class="outer"><button class="b"><span class="s">x</span></button><a class="a">y</a></div>', 0)() as Element);
    const button = n.firstChild as Element;
    const a = n.lastChild as Element;
    $.on(n, "click", () => log.push("outer"), 0);
    $.on(button, "click", () => log.push("button-self"), SELF);
    $.on(button, "click", () => log.push("button-stop"), STOP | PREVENT);
    $.on(a, "click", (ev) => log.push(`synthetic ${ev.defaultPrevented}`), SYNTHETIC | PREVENT);
    $.on(n, "click", () => log.push("outer-synthetic"), SYNTHETIC);
    $.on(n, "click", () => log.push("outer-capture"), CAPTURE);
    void $self;
    return n;
  });
  const { root } = await mountApp(C, [t]);
  const ev = click(root.querySelector(".s")!);
  // capture first; self skips (target is the span); stop ends bubbling before .outer
  expect(log).toEqual(["outer-capture", "button-stop"]);
  expect(ev.defaultPrevented).toBe(true);
  log.length = 0;
  click(root.querySelector(".b")!);
  expect(log).toEqual(["outer-capture", "button-self", "button-stop"]);
  log.length = 0;
  click(root.querySelector(".a")!);
  expect(log).toEqual(["outer-capture", "outer", "synthetic true", "outer-synthetic"]);
});

test("a delegated stop ends the walk to the root", async () => {
  const log: string[] = [];
  const { C, t } = comp("T", () => {
    const n = ($.template("<div><p></p></div>", 0)() as Element);
    $.on(n.firstChild as Element, "click", () => log.push("inner"), SYNTHETIC | STOP);
    $.on(n, "click", () => log.push("outer"), SYNTHETIC);
    return n;
  });
  const { root } = await mountApp(C, [t]);
  click(root.querySelector("p")!);
  expect(log).toEqual(["inner"]);
});

/// (partial: runtime half, the invalid handler expression error)
test("callHandler calls a function with the component as this and refuses anything else with OWL's message", async () => {
  const errors: unknown[] = [];
  let seenThis: unknown;
  const value = signal<unknown>(function (this: unknown) {
    seenThis = this;
  });
  const { C, t } = comp("T", ($self) => {
    const n = ($.template("<button></button>", 0)() as Element);
    $.on(n, "click", ($e) => $.callHandler(value(), $self, $e), 0);
    return n;
  });
  const { root, instance } = await mountApp(C, [t], { errors });
  click(root.querySelector("button")!);
  expect(seenThis).toBe(instance);
  value.set("this.save()");
  click(root.querySelector("button")!);
  expect(errors).toHaveLength(1);
  expect((errors[0] as Error).message).toBe(
    "Invalid handler expression: the `t-on` expression should evaluate to a function, but got 'string'. Did you mean to use an arrow function? (e.g. `t-on-click=\"() => expr\"`)",
  );
});

test("callHandler: a string expression is refused, never evaluated (security §8)", () => {
  const g = globalThis as { erpHandlerRan?: boolean };
  expect(() => $.callHandler("globalThis.erpHandlerRan = true", {}, new Event("click"))).toThrow(
    /should evaluate to a function, but got 'string'/,
  );
  expect(g.erpHandlerRan).toBeUndefined();
});

test("on: a string handler is refused, never evaluated (security §8)", async () => {
  const g = globalThis as { erpOnRan?: boolean };
  const errors: unknown[] = [];
  const { C, t } = comp("T", () => {
    const n = ($.template("<button></button>", 0)() as Element);
    ($.on as (el: Element, e: string, h: unknown, f: number) => void)(n, "click", "globalThis.erpOnRan = true", 0);
    return n;
  });
  await expect(mountApp(C, [t], { errors })).rejects.toThrow(UIError);
  expect(errors[0]).toBeInstanceOf(UIError);
  expect(g.erpOnRan).toBeUndefined();
});

test("a handler error goes to the owner's error chain, and a removed block's handlers never run", async () => {
  const errors: unknown[] = [];
  const show = signal(true);
  let ran = 0;
  let kept: Element | null = null;
  const { C, t } = comp("T", () => {
    const n = ($.template("<div><!></div>", 0)() as Element);
    $.cond(n, n.firstChild, () => (show() ? 0 : -1), [
      () => {
        const b = ($.template("<button></button>", 0)() as Element);
        kept = b as Element;
        $.on(b as Element, "click", () => {
          ran++;
          throw new Error("handler failed");
        }, 0);
        return b;
      },
    ]);
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  click(root.querySelector("button")!);
  expect(ran).toBe(1);
  expect((errors[0] as Error).message).toBe("handler failed");
  show.set(false);
  flushSync();
  expect(root.querySelector("button")).toBeNull();
  click(kept!);
  expect(ran).toBe(1);
  expect(errors).toHaveLength(1);
});

test("a handler that returns a rejected promise reaches the owner's error chain, direct or delegated (review M5)", async () => {
  const errors: unknown[] = [];
  const { C, t } = comp("T", ($self) => {
    const n = ($.template("<button></button>", 0)() as Element);
    $.on(n, "click", () => Promise.reject(new Error("async handler failed")), 0);
    $.on(n, "click", ($e) => $.callHandler(async () => {
      throw new Error("async method failed");
    }, $self, $e), SYNTHETIC);
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  click(root.querySelector("button")!);
  await settle();
  expect(errors.map((e) => (e as Error).message)).toEqual(["async handler failed", "async method failed"]);
});

test("delegated listeners are keyed by event, capture and passive: a later non-passive handler still prevents (review M11)", async () => {
  const seen: boolean[] = [];
  const { C, t } = comp("T", () => {
    const n = ($.template("<div><a></a></div>", 0)() as Element);
    $.on(n, "wheel", () => {}, SYNTHETIC | PASSIVE);
    $.on(n.firstChild as Element, "wheel", (ev) => seen.push(ev.defaultPrevented), SYNTHETIC | PREVENT);
    return n;
  });
  const { root } = await mountApp(C, [t]);
  const a = root.querySelector("a")!;
  const ev = new Event("wheel", { bubbles: true, cancelable: true });
  a.dispatchEvent(ev);
  expect(seen).toEqual([true]);
  expect(ev.defaultPrevented).toBe(true);
  expect(Object.keys(a)).toEqual([]); // handlers live in a module map, not on the element
});

test("a passive direct listener cannot prevent the default action", async () => {
  const { C, t } = comp("T", () => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.on(n, "touchstart", (ev) => ev.preventDefault(), PASSIVE);
    return n;
  });
  const { root } = await mountApp(C, [t]);
  const ev = new Event("touchstart", { bubbles: true, cancelable: true });
  root.querySelector("div")!.dispatchEvent(ev);
  expect(ev.defaultPrevented).toBe(false);
});

test("capture listeners, direct and delegated, run before bubbling ones; a delegated handler's currentTarget is the root", async () => {
  const log: string[] = [];
  let current: EventTarget | null = null;
  const { C, t } = comp("T", () => {
    const n = ($.template("<div><span></span></div>", 0)() as Element);
    $.on(n, "click", () => log.push("direct bubble"), 0);
    $.on(n, "click", () => log.push("direct capture"), CAPTURE);
    $.on(n, "click", (ev) => {
      log.push("delegated bubble");
      current = ev.currentTarget;
    }, SYNTHETIC);
    $.on(n, "click", () => log.push("delegated capture"), SYNTHETIC | CAPTURE);
    return n;
  });
  const { root } = await mountApp(C, [t]);
  click(root.querySelector("span")!);
  expect(log).toEqual(["delegated capture", "direct capture", "direct bubble", "delegated bubble"]);
  expect(current).toBe(root);
});

test("on refuses an event name outside [a-z0-9_.:-] before anything is registered (security review L3)", async () => {
  const el = document.createElement("div");
  for (const bad of ["Click", "on click", "click<", "", "\u0130"]) {
    expect(() => $.on(el, bad, () => {}, SYNTHETIC)).toThrow(
      new UIError(`on: ${JSON.stringify(bad)} is not an event name: use lowercase letters, digits, "_", ".", ":" and "-"`, "value"),
    );
  }
  const log: string[] = [];
  const { C, t } = comp("T", () => {
    const n = ($.template("<div></div>", 0)() as Element);
    for (const ok of ["update:value", "my-event", "a.b_c1"]) $.on(n, ok, () => log.push(ok), SYNTHETIC);
    return n;
  });
  const { root } = await mountApp(C, [t]);
  root.querySelector("div")!.dispatchEvent(new Event("update:value", { bubbles: true }));
  expect(log).toEqual(["update:value"]);
});

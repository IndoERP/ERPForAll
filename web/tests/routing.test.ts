/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Error routing and refusals added by review round 2: every
// error reaches an owner's chain or the app's report channel, also after its owner is gone, and inputs
// that would silently do nothing are refused. Each test fails on 311e24e6.

import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  Component,
  ErrorBoundary,
  Pending,
  ReactiveLoopError,
  type Signal,
  UIError,
  UnsafeAttributeError,
  effect,
  flushSync,
  onError,
  onMounted,
  onWillStart,
  onWillUnmount,
  signal,
  unmount,
  useEffect,
} from "../src/index.js";
import { PASSIVE, PREVENT, SYNTHETIC } from "../src/runtime/events.js";
import { type Owner, dispose, getOwner, setOwner } from "../src/runtime/owner.js";
import { createEffect, limits } from "../src/runtime/reactive.js";
import config from "../vitest.config.js";
import { comp, mountApp, settle, tpl } from "./harness.js";

function click(el: Element): void {
  el.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
}

function messages(errors: unknown[]): string[] {
  return errors.map((e) => (e as Error).message);
}

/** A promise and its reject function. */
function deferred(): { promise: Promise<void>; resolve: () => void; reject: (e: unknown) => void } {
  let resolve!: () => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<void>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** Runs `fn` with `globalThis.reportError` collecting into `reported`. */
async function withReportError(reported: unknown[], fn: () => Promise<void> | void): Promise<void> {
  const g = globalThis as { reportError?: (e: unknown) => void };
  const saved = g.reportError;
  g.reportError = (e) => reported.push(e);
  try {
    await fn();
  } finally {
    g.reportError = saved;
  }
}

// --- silent-failure 1: an error after the owner is gone is reported -------------------------------

test("a handler promise that rejects after unmount reaches the mount's onError (round 2, silent-failure 1)", async () => {
  const errors: unknown[] = [];
  const save = deferred();
  const { C, t } = comp("T", () => {
    const n = $.template("<button></button>", 0)() as Element;
    $.on(n, "click", () => save.promise, 0);
    return n;
  });
  const { root } = await mountApp(C, [t], { errors });
  click(root.querySelector("button")!);
  unmount(root);
  save.reject(new Error("validation failed"));
  await settle();
  expect(messages(errors)).toEqual(["validation failed"]);
});

test("a delegated handler promise that rejects after its branch was removed is reported (round 2, silent-failure 1)", async () => {
  const errors: unknown[] = [];
  const pageErrors: unknown[] = [];
  const save = deferred();
  const show = signal(true);
  const { C, t } = comp("T", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.cond(n, null, () => (show() ? 0 : 1), [
      () => {
        const b = $.template("<button></button>", 0)() as Element;
        $.on(b, "click", ($e) => $.callHandler(() => save.promise, $self, $e), SYNTHETIC);
        return b;
      },
      () => null,
    ]);
    return n;
  }, { setup: () => onError((e) => void pageErrors.push(e)) });
  const { root } = await mountApp(C, [t], { errors });
  click(root.querySelector("button")!);
  show.set(false);
  flushSync();
  save.reject(new Error("late failure"));
  await settle();
  // The branch's owner is gone: the app's report channel gets it, not a handler of a living component.
  expect(messages(errors)).toEqual(["late failure"]);
  expect(pageErrors).toEqual([]);
});

test("an onWillStart that rejects after unmount is reported (round 2, silent-failure 1)", async () => {
  const errors: unknown[] = [];
  const load = deferred();
  const { C: Slow, t: slowT } = comp("Slow", () => null, { setup: () => onWillStart(() => load.promise) });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "Slow", {}, null, null);
    return n;
  }, { components: { Slow } });
  // The slow child is created after the mount resolved, in a branch, so the mount does not wait for it.
  const show = signal(false);
  const { C: Root, t: rootT } = comp("Root", ($self) => {
    const n = $.template("<div></div>", 0)() as Element;
    $.cond(n, null, () => (show() ? 0 : 1), [
      () => {
        const d = $.template("<section></section>", 0)() as Element;
        $.component(d, null, $self, "Page", {}, null, null);
        return d;
      },
      () => null,
    ]);
    return n;
  }, { components: { Page: C } });
  const { root } = await mountApp(Root, [rootT, t, slowT], { errors });
  show.set(true);
  flushSync();
  unmount(root);
  load.reject(new Error("load failed"));
  await settle();
  expect(messages(errors)).toEqual(["load failed"]);
});

// --- silent-failure 2: a boundary being disposed never takes an error ----------------------------

test("an ErrorBoundary being disposed passes a teardown error on and builds no fallback (round 2, silent-failure 2)", async () => {
  const errors: unknown[] = [];
  let fallbacks = 0;
  const mode = signal(0);
  const { C: Bad, t: badT } = comp("Bad", () => $.template("<b>bad</b>", 0)() as Element, {
    setup: () =>
      onWillUnmount(() => {
        throw new Error("unmount failed");
      }),
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.cond(n, null, () => mode(), [
      () => {
        const d = $.template("<div></div>", 0)() as Element;
        $.component(d, null, $self, "ErrorBoundary", {}, null, {
          default: {
            render: () => {
              const s = $.template("<section></section>", 0)() as Element;
              $.component(s, null, $self, "Bad", {}, null, null);
              return s;
            },
          },
          fallback: {
            render: () => {
              fallbacks++;
              return $.template("<i>fallback</i>", 0)() as Element;
            },
          },
        });
        return d;
      },
      () => $.template("<p>other</p>", 0)() as Element,
    ]);
    return n;
  }, { components: { Bad, ErrorBoundary }, setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t, badT]);
  mode.set(1);
  flushSync();
  expect(messages(errors)).toEqual(["unmount failed"]);
  expect(fallbacks).toBe(0);
  expect(root.innerHTML).toBe("<main><p>other</p></main>");
});

test("on unmount, a teardown error inside an ErrorBoundary reaches the mount's onError (round 2, silent-failure 2)", async () => {
  const errors: unknown[] = [];
  let fallbacks = 0;
  const { C: Bad, t: badT } = comp("Bad", () => $.template("<b>bad</b>", 0)() as Element, {
    setup: () =>
      onWillUnmount(() => {
        throw new Error("unmount failed");
      }),
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "ErrorBoundary", {}, null, {
      default: {
        render: () => {
          const s = $.template("<section></section>", 0)() as Element;
          $.component(s, null, $self, "Bad", {}, null, null);
          return s;
        },
      },
      fallback: {
        render: () => {
          fallbacks++;
          return null;
        },
      },
    });
    return n;
  }, { components: { Bad, ErrorBoundary } });
  const { root } = await mountApp(C, [t, badT], { errors });
  unmount(root);
  expect(messages(errors)).toEqual(["unmount failed"]);
  expect(fallbacks).toBe(0);
  expect(root.innerHTML).toBe("");
});

// --- silent-failure 3: promises of hooks, effects, setup and onError --------------------------------

test("async useEffect, onMounted and onWillUnmount rejections reach the error chain or the report (round 2, silent-failure 3)", async () => {
  const errors: unknown[] = [];
  const pageErrors: unknown[] = [];
  const { C, t } = comp("T", () => null, {
    setup: () => {
      onError((e) => void pageErrors.push(e));
      useEffect((async () => {
        throw new Error("effect failed");
      }) as unknown as () => void);
      onMounted((async () => {
        throw new Error("mounted failed");
      }) as unknown as () => void);
      onWillUnmount((async () => {
        throw new Error("unmount failed");
      }) as unknown as () => void);
    },
  });
  const { root } = await mountApp(C, [t], { errors });
  await settle();
  expect(messages(pageErrors)).toEqual(["effect failed", "mounted failed"]);
  unmount(root);
  await settle();
  // The component is gone when its willUnmount promise rejects: reported through the app.
  expect(messages(errors)).toEqual(["unmount failed"]);
});

test("an async public effect's rejection reaches its owner's chain (round 2, silent-failure 3)", async () => {
  const errors: unknown[] = [];
  let owner: Owner | null = null;
  const { C, t } = comp("T", () => null, {
    setup: () => {
      owner = getOwner();
      onError((e) => void errors.push(e));
    },
  });
  await mountApp(C, [t]);
  const prev = setOwner(owner);
  try {
    effect((async () => {
      throw new Error("async effect failed");
    }) as unknown as () => void);
  } finally {
    setOwner(prev);
  }
  await settle();
  expect(messages(errors)).toEqual(["async effect failed"]);
});

test("an async setup() is refused with a usage error, and its own rejection is reported (round 2, silent-failure 3)", async () => {
  const errors: unknown[] = [];
  const pageErrors: unknown[] = [];
  class AsyncSetup extends Component {
    static override template = "AsyncSetup";
    override setup(): void {
      return Promise.reject(new Error("setup body failed")) as unknown as void;
    }
  }
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "AsyncSetup", {}, null, null);
    return n;
  }, { components: { AsyncSetup }, setup: () => onError((e) => void pageErrors.push(e)) });
  await mountApp(C, [t, tpl("AsyncSetup", () => null)], { errors });
  await settle();
  expect(pageErrors).toEqual([
    new UIError("setup() of AsyncSetup must be synchronous; move async work to onWillStart", "usage"),
  ]);
  expect((pageErrors[0] as UIError).code).toBe("usage");
  expect(messages(errors)).toEqual(["setup body failed"]);
});

test("an async onError handler has not handled the error: a usage error carrying it goes up (round 2, silent-failure 3)", async () => {
  const pageErrors: unknown[] = [];
  const { C: Child, t: childT } = comp("Child", () => null, {
    setup: () => {
      onError((async () => {}) as unknown as () => void);
      useEffect(() => {
        throw new Error("child failed");
      });
    },
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "Child", {}, null, null);
    return n;
  }, { components: { Child }, setup: () => onError((e) => void pageErrors.push(e)) });
  await mountApp(C, [t, childT]);
  expect(pageErrors).toHaveLength(1);
  const e = pageErrors[0] as UIError;
  expect(e).toBeInstanceOf(UIError);
  expect(e.code).toBe("usage");
  expect(e.message).toBe(
    "an onError handler of Child returned a promise: onError handlers must be synchronous (the error is this error's cause)",
  );
  expect((e.cause as Error).message).toBe("child failed");
});

// --- silent-failure 4: the continuation of an async onWillStart ------------------------------------

test("a removed anchor after onWillStart resolves reaches the chain, and the mount still settles (round 2, silent-failure 4)", async () => {
  const errors: unknown[] = [];
  const load = deferred();
  let marker: Element | null = null;
  const { C: Slow, t: slowT } = comp("Slow", () => $.template("<b>slow</b>", 0)() as Element, {
    setup: () => onWillStart(() => load.promise),
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main><span></span></main>", 0)() as Element;
    marker = n.firstChild as Element;
    $.component(n, marker, $self, "Slow", {}, null, null);
    return n;
  }, { components: { Slow }, setup: () => onError((e) => void errors.push(e)) });
  const mounted = mountApp(C, [t, slowT]);
  await settle();
  marker!.remove();
  load.resolve();
  const { root } = await mounted;
  expect(errors).toEqual([
    new UIError("the anchor of a dynamic position was removed from the DOM by code outside the runtime", "detached"),
  ]);
  expect(root.innerHTML).toBe("<main></main>");
});

test("a Pending whose commit fails reports the error to its chain, not as an unhandled rejection (round 2, silent-failure 4)", async () => {
  const errors: unknown[] = [];
  const load = deferred();
  let marker: Element | null = null;
  const { C: Slow, t: slowT } = comp("Slow", () => $.template("<b>slow</b>", 0)() as Element, {
    setup: () => onWillStart(() => load.promise),
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main><span></span></main>", 0)() as Element;
    marker = n.firstChild as Element;
    $.component(n, marker, $self, "Pending", {}, null, {
      default: {
        render: () => {
          const d = $.template("<div></div>", 0)() as Element;
          $.component(d, null, $self, "Slow", {}, null, null);
          return d;
        },
      },
    });
    return n;
  }, { components: { Slow, Pending }, setup: () => onError((e) => void errors.push(e)) });
  await mountApp(C, [t, slowT]);
  marker!.remove();
  load.resolve();
  await settle();
  expect(errors).toEqual([
    new UIError("the anchor of a dynamic position was removed from the DOM by code outside the runtime", "detached"),
  ]);
});

// --- silent-failure 5: every onWillStart rejection is seen ----------------------------------------

test("every onWillStart rejection is seen: the first goes to the chain, later ones are reported (round 2, silent-failure 5)", async () => {
  const errors: unknown[] = [];
  const pageErrors: unknown[] = [];
  const a = deferred();
  const b = deferred();
  const { C: Slow, t: slowT } = comp("Slow", () => null, {
    setup: () => {
      onWillStart(() => a.promise);
      onWillStart(() => b.promise);
    },
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "Pending", {}, null, {
      default: {
        render: () => {
          const d = $.template("<div></div>", 0)() as Element;
          $.component(d, null, $self, "Slow", {}, null, null);
          return d;
        },
      },
    });
    return n;
  }, { components: { Slow, Pending }, setup: () => onError((e) => void pageErrors.push(e)) });
  await mountApp(C, [t, slowT], { errors });
  a.reject(new Error("first rpc failed"));
  await settle();
  b.reject(new Error("second rpc failed"));
  await settle();
  expect(messages(pageErrors)).toEqual(["first rpc failed"]);
  expect(messages(errors)).toEqual(["second rpc failed"]);
});

test("when a later onWillStart throws, an earlier promise's rejection is reported (round 2, silent-failure 5)", async () => {
  const errors: unknown[] = [];
  const pageErrors: unknown[] = [];
  const a = deferred();
  const { C: Slow, t: slowT } = comp("Slow", () => null, {
    setup: () => {
      onWillStart(() => a.promise);
      onWillStart(() => {
        throw new Error("second hook failed");
      });
    },
  });
  const { C, t } = comp("Page", ($self) => {
    const n = $.template("<main></main>", 0)() as Element;
    $.component(n, null, $self, "Slow", {}, null, null);
    return n;
  }, { components: { Slow }, setup: () => onError((e) => void pageErrors.push(e)) });
  await mountApp(C, [t, slowT], { errors });
  a.reject(new Error("first hook failed"));
  await settle();
  expect(messages(pageErrors)).toEqual(["second hook failed"]);
  expect(messages(errors)).toEqual(["first hook failed"]);
});

// --- silent-failure 6: nodes moved by code outside the runtime -------------------------------------

test("a branch whose nodes code outside the runtime removed fails loudly when it is replaced (round 2, silent-failure 6)", async () => {
  const errors: unknown[] = [];
  const mode = signal(0);
  const { C, t } = comp("T", () => {
    const n = $.template("<main></main>", 0)() as Element;
    $.cond(n, null, () => mode(), [
      () => $.template("<p>a</p>", 0)() as Element,
      () => $.template("<p>b</p>", 0)() as Element,
    ]);
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  root.querySelector("p")!.remove(); // e.g. a browser translation replaced it
  mode.set(1);
  flushSync();
  expect(errors).toEqual([
    new UIError("the nodes of a block were moved or removed from the DOM by code outside the runtime", "detached"),
  ]);
});

test("a range whose last node is no longer after its first fails before anything is removed (round 2, silent-failure 6)", async () => {
  const errors: unknown[] = [];
  const mode = signal(0);
  const { C, t } = comp("T", () => {
    const n = $.template("<main></main>", 0)() as Element;
    $.cond(n, null, () => mode(), [
      () => $.template("<p>1</p><p>2</p>", 1)(),
      () => null,
    ]);
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  const main = root.querySelector("main")!;
  const second = main.lastChild!;
  document.body.appendChild(second); // moved elsewhere
  mode.set(1);
  flushSync();
  expect((errors[0] as UIError).code).toBe("detached");
  expect(main.textContent).toBe("1"); // nothing was removed
  second.parentNode!.removeChild(second);
});

test("a text output whose node code outside the runtime removed fails loudly (round 2, silent-failure 6)", async () => {
  const errors: unknown[] = [];
  const value = signal<string | null>("x");
  const { C, t } = comp("T", () => {
    const n = $.template("<p></p>", 0)() as Element;
    $.insert(n, () => value(), null);
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  root.querySelector("p")!.firstChild!.remove();
  value.set(null);
  flushSync();
  expect(errors).toEqual([
    new UIError("the nodes of a block were moved or removed from the DOM by code outside the runtime", "detached"),
  ]);
});

// --- silent-failure 7: test.only and test.skip ------------------------------------------------------

test("vitest refuses a committed test.only on every host (round 2, silent-failure 7)", () => {
  expect(config.test?.allowOnly).toBe(false);
});

// --- silent-failure 8: prevent with passive ---------------------------------------------------------

test("on refuses prevent together with passive, direct or delegated (round 2, silent-failure 8)", async () => {
  const errors: unknown[] = [];
  const { C, t } = comp("T", () => {
    const n = $.template("<div></div>", 0)() as Element;
    for (const flags of [PREVENT | PASSIVE, PREVENT | PASSIVE | SYNTHETIC]) {
      try {
        $.on(n, "touchstart", () => {}, flags);
      } catch (e) {
        errors.push(e);
      }
    }
    return n;
  });
  await mountApp(C, [t]);
  const expected = new UIError(
    'on: "touchstart" has both prevent and passive: a passive listener cannot prevent the default action; drop one of them',
    "value",
  );
  expect(errors).toEqual([expected, expected]);
  expect((errors[0] as UIError).code).toBe("value");
});

// --- silent-failure 9: the flush backstop ------------------------------------------------------------

test("the flush backstop stops every app that lost queued effects, whatever boundaries exist (round 2, silent-failure 9)", async () => {
  const errors: unknown[] = [];
  const handled: unknown[] = [];
  let owner: Owner | null = null;
  const { C, t } = comp("O", () => $.template("<p>app</p>", 0)() as Element, {
    setup: () => {
      owner = getOwner();
      onError((e) => void handled.push(e)); // would swallow the error before round 2
    },
  });
  const { root } = await mountApp(C, [t], { errors });
  const saved = { ...limits };
  limits.runsPerFlush = 50;
  try {
    const spawn = (): Signal<number> => {
      const s = signal(0);
      createEffect(() => {
        if (s() > 0) spawn().set(1);
      }, owner!);
      return s;
    };
    spawn().set(1);
    flushSync();
  } finally {
    Object.assign(limits, saved);
  }
  expect(handled).toEqual([]);
  expect(errors).toHaveLength(1);
  expect(errors[0]).toBeInstanceOf(ReactiveLoopError);
  expect((errors[0] as Error).message).toBe(
    "a flush ran more than 50 effects without repeating one: effects keep creating and waking new effects; the queued effects were dropped and the app stopped",
  );
  expect(root.querySelector(".o_erp_crash")).not.toBeNull();
});

// --- silent-failure 10: effect() with no living owner --------------------------------------------

test("effect() under a disposed owner is refused (round 2, silent-failure 10)", async () => {
  let owner: Owner | null = null;
  const { C, t } = comp("T", () => null, { setup: () => void (owner = getOwner()) });
  await mountApp(C, [t]);
  const child = owner!.child();
  dispose(child);
  const prev = setOwner(child);
  try {
    expect(() => effect(() => {})).toThrow(
      new UIError("effect() was called under a component that is already destroyed: the effect would never run", "usage"),
    );
  } finally {
    setOwner(prev);
  }
});

test("effect() from an event handler or a lifecycle hook is refused (round 2, silent-failure 10)", async () => {
  const errors: unknown[] = [];
  const { C, t } = comp("T", () => {
    const n = $.template("<button></button>", 0)() as Element;
    $.on(n, "click", () => {
      effect(() => {});
    }, 0);
    return n;
  }, {
    setup: () => {
      onError((e) => void errors.push(e));
      onMounted(() => {
        effect(() => {});
      });
    },
  });
  const { root } = await mountApp(C, [t]);
  click(root.querySelector("button")!);
  const expected = new UIError(
    "effect() was called from an event handler or a lifecycle hook, where no component owns it: create it in setup() with useEffect, or while rendering",
    "usage",
  );
  expect(errors).toEqual([expected, expected]);
  // Outside every component it is still a detached effect the caller stops.
  let runs = 0;
  const stop = effect(() => {
    runs++;
  });
  stop();
  expect(runs).toBe(1);
});

// --- silent-failure 11: the delegation key -----------------------------------------------------------

test("a delegated listener whose root cannot be found fails every time, not only the first (round 2, silent-failure 11)", async () => {
  const reported: unknown[] = [];
  await withReportError(reported, () => {
    const el = document.createElement("button");
    const stops = [0, 1].map(() =>
      effect(() => {
        $.on(el, "dblclick", () => {}, SYNTHETIC);
      }),
    );
    for (const stop of stops) stop();
  });
  expect(reported).toEqual([
    new UIError("an effect created outside a component has no render root", "usage"),
    new UIError("an effect created outside a component has no render root", "usage"),
  ]);
});

// --- silent-failure 12: malformed style declarations -------------------------------------------------

test("a style declaration without a property name is refused (round 2, silent-failure 12)", async () => {
  const errors: unknown[] = [];
  const value = signal("color: red");
  const { C, t } = comp("T", () => {
    const n = $.template("<p></p>", 0)() as Element;
    $.effect(() => $.style(n, value()));
    return n;
  }, { setup: () => onError((e) => void errors.push(e)) });
  const { root } = await mountApp(C, [t]);
  expect(root.querySelector("p")!.getAttribute("style")).toBe("color: red;");
  value.set("color red");
  flushSync();
  value.set(": red; width: 1px");
  flushSync();
  value.set("width: 1px;; ");
  flushSync();
  expect(errors).toEqual([
    new UIError('style: "color red" is not a declaration "property: value"', "value"),
    new UIError('style: ": red" is not a declaration "property: value"', "value"),
  ]);
  expect(root.querySelector("p")!.getAttribute("style")).toBe("width: 1px;");
});

// --- type review L1: the subclass codes are reserved ------------------------------------------------

test("a plain UIError cannot carry a subclass's code (type review L1)", () => {
  const make = UIError as unknown as new (message: string, code: string) => UIError;
  expect(() => new make("x", "unsafe-attribute")).toThrow(
    new UIError('UIError: the code "unsafe-attribute" belongs to its own error class; use one of the base codes', "usage"),
  );
  expect(() => new make("x", "reactive-loop")).toThrow(UIError);
  expect(new UnsafeAttributeError("y").code).toBe("unsafe-attribute");
  expect(new ReactiveLoopError("z").code).toBe("reactive-loop");
  expect(new UIError("w", "value", { cause: 1 }).cause).toBe(1);
});

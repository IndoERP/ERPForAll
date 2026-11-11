/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Components, props, lifecycle, slots and boundaries (§7.5, §7.6).

import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  Component,
  type ComponentClass,
  ErrorBoundary,
  Pending,
  UIError,
  effect,
  flushSync,
  mount,
  onError,
  onMounted,
  onWillDestroy,
  onWillStart,
  onWillUnmount,
  signal,
  unmount,
  useEffect,
} from "../src/index.js";
import { SYNTHETIC } from "../src/runtime/events.js";
import { comp, mountApp, settle, target, tpl } from "./harness.js";

test("a component class renders its static template after setup ran once with the props", async () => {
  const log: string[] = [];
  class Root extends Component<{ name: string }> {
    static override template = "Root";
    greeting = "";
    override setup(): void {
      log.push(`setup ${this.props.name}`);
      this.greeting = `Hello ${this.props.name}`;
    }
  }
  const t = tpl("Root", ($self) => {
    log.push("render");
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => $self.greeting, null);
    return n;
  });
  const { root, instance } = await mountApp(Root, [t], { props: { name: "Ana" } });
  expect(log).toEqual(["setup Ana", "render"]);
  expect(instance).toBeInstanceOf(Root);
  expect(root.innerHTML).toBe("<p>Hello Ana</p>");
});

test("a hook outside setup is an error naming the hook", () => {
  expect(() => onMounted(() => {})).toThrow(new UIError("onMounted can only be called during setup()", "usage"));
  expect(() => onWillStart(() => {})).toThrow(/onWillStart can only be called during setup\(\)/);
});

test("a missing child class or template is an error naming it", async () => {
  const errors: unknown[] = [];
  const { C, t } = comp("P", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Missing", {}, null, null);
    return n;
  });
  await mountApp(C, [t], { errors }).catch(() => {});
  expect((errors[0] as Error).message).toBe('Cannot find the definition of component "Missing" in P.components');
  class NoTemplate extends Component {
    static override template = "nope";
  }
  const errors2: unknown[] = [];
  await mountApp(NoTemplate, [], { errors: errors2 }).catch(() => {});
  expect((errors2[0] as Error).message).toBe('template "nope" of component NoTemplate is not registered');
});

test("props are signal-backed getters: a binding re-runs only for the props it reads", async () => {
  const a = signal(1);
  const b = signal(10);
  let aRuns = 0;
  const child = comp("Child", ($self) => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => {
      aRuns++;
      return $self.props.a;
    }, null);
    return n;
  });
  let props: Record<string, unknown> = {};
  const parent = comp(
    "Parent",
    ($self) => {
      const n = ($.template("<div></div>", 0)() as Element);
      $.component(n, null, $self, "Child", { a: () => a(), b: () => b(), k: () => "constant" }, null, null);
      return n;
    },
    { components: { Child: child.C }, setup: () => {} },
  );
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.textContent).toBe("1");
  b.set(11);
  flushSync();
  expect(aRuns).toBe(1);
  a.set(2);
  flushSync();
  expect(aRuns).toBe(2);
  expect(root.textContent).toBe("2");
  // enumerable getters, one per key
  const C2 = class extends Component<Record<string, unknown>> {
    static override template = "C2";
    override setup(): void {
      props = this.props;
    }
  };
  const p2 = comp("P2", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "C2", { x: () => 1, y: () => 2 }, null, null);
    return n;
  }, { components: { C2 } });
  await mountApp(p2.C, [p2.t, tpl("C2", () => null)]);
  expect(Object.keys(props)).toEqual(["x", "y"]);
  expect(typeof Object.getOwnPropertyDescriptor(props, "x")!.get).toBe("function");
});

test("a t-props spread adds and deletes keys, under the explicit props, and readers of new keys update", async () => {
  const spread = signal<Record<string, unknown>>({ a: 1, b: 2 });
  let props: Record<string, unknown> = {};
  const child = comp("Child", ($self) => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => `${String($self.props.b)}|${String($self.props.c)}|${Object.keys($self.props).join(",")}`, null);
    return n;
  }, { setup: (self) => (props = self.props) });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", { b: () => "explicit" }, () => spread(), null);
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.textContent).toBe("explicit|undefined|b,a");
  spread.set({ c: 3 });
  flushSync();
  expect(root.textContent).toBe("explicit|3|b,c");
  expect("a" in props).toBe(false);
});

test("an async onWillStart holds the component's subtree until it resolves", async () => {
  let resolve!: () => void;
  const log: string[] = [];
  const child = comp("Child", () => ($.template("<b>child</b>", 0)() as Element), {
    setup: () => {
      onWillStart(() => new Promise<void>((r) => (resolve = r)));
      onMounted(() => void log.push("child mounted"));
    },
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div><!>after</div>", 0)() as Element);
    $.component(n, n.firstChild, $self, "Child", {}, null, null);
    return n;
  }, { components: { Child: child.C }, setup: () => onMounted(() => void log.push("parent mounted")) });
  const root = target();
  let mounted = false;
  const done = mount(parent.C, root, { templates: [parent.t, child.t] }).then(() => (mounted = true));
  await settle();
  expect(mounted).toBe(false);
  expect(root.innerHTML).toBe(""); // the root boundary holds everything
  resolve();
  await done;
  expect(root.textContent).toBe("childafter");
  expect(log).toEqual(["child mounted", "parent mounted"]);
  unmount(root);
});

test("onMounted runs after the DOM is in the document, children before parents", async () => {
  const log: string[] = [];
  const make = (name: string, kids: string[], components: Record<string, ComponentClass> = {}) =>
    comp(name, ($self) => {
      const n = ($.template("<div></div>", 0)() as Element);
      for (const k of kids) $.component(n, null, $self, k, {}, null, null);
      return n;
    }, {
      components,
      setup: () => onMounted(() => void log.push(name)),
    });
  const a1 = make("A1", []);
  const a = make("A", ["A1"], { A1: a1.C });
  const b = make("B", []);
  const root = make("Root", ["A", "B"], { A: a.C, B: b.C });
  let connected = false;
  const probe = comp("Probe", () => ($.template("<i></i>", 0)() as Element), {
    setup: () => onMounted(() => void (connected = document.body.contains(document.querySelector("i")))),
  });
  const shell = comp("Shell", ($self) => {
    const n = ($.template("<section></section>", 0)() as Element);
    $.component(n, null, $self, "Root", {}, null, null);
    $.component(n, null, $self, "Probe", {}, null, null);
    return n;
  }, { components: { Root: root.C, Probe: probe.C } });
  await mountApp(shell.C, [a1.t, a.t, b.t, root.t, probe.t, shell.t]);
  expect(log).toEqual(["A1", "A", "B", "Root"]);
  expect(connected).toBe(true);
});

test("onWillUnmount runs in reverse registration order while the DOM is still attached", async () => {
  const show = signal(true);
  const log: string[] = [];
  const child = comp("Child", () => ($.template('<b class="c">c</b>', 0)() as Element), {
    setup: () => {
      onWillUnmount(() => void log.push(`first ${document.querySelector(".c") !== null}`));
      onWillUnmount(() => void log.push(`second ${document.querySelector(".c") !== null}`));
    },
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div><!></div>", 0)() as Element);
    $.cond(n, n.firstChild, () => (show() ? 0 : -1), [
      () => {
        const f = ($.template("<span></span>", 0)() as Element);
        $.component(f, null, $self, "Child", {}, null, null);
        return f;
      },
    ]);
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  show.set(false);
  flushSync();
  expect(log).toEqual(["second true", "first true"]);
  expect(root.querySelector(".c")).toBeNull();
});

test("onWillDestroy runs at destruction in reverse order, children before parents, and stops their effects", async () => {
  const log: string[] = [];
  const s = signal(0);
  const child = comp("Child", () => null, {
    setup: () => {
      effect(() => {
        log.push(`effect ${s()}`);
      });
      onWillDestroy(() => void log.push("child destroy"));
    },
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", {}, null, null);
    return n;
  }, {
    components: { Child: child.C },
    setup: () => {
      onWillDestroy(() => void log.push("parent destroy 1"));
      onWillDestroy(() => void log.push("parent destroy 2"));
    },
  });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  unmount(root);
  expect(log).toEqual(["effect 0", "child destroy", "parent destroy 2", "parent destroy 1"]);
  s.set(1);
  flushSync();
  expect(log).toHaveLength(4);
  expect(root.innerHTML).toBe("");
});

test("a default slot renders the caller's content, or the fallback when there is none", async () => {
  const child = comp("Child", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.slot(n, null, $self, "default", null, () => ($.template("<p>Fallback</p>", 0)() as Element));
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const f = $.template("<!><!>", 1)();
    const m0 = f.firstChild!;
    const m1 = m0.nextSibling!;
    $.component(f, m0, $self, "Child", {}, null, {
      default: {
        render: () => {
          const b = ($.template("<b></b>", 0)() as Element);
          $.insert(b, () => $self.label, null);
          return b;
        },
      },
    });
    $.component(f, m1, $self, "Child", {}, null, null);
    return f;
  }, { components: { Child: child.C }, setup: (self) => (self.label = "caller's") });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  const divs = root.querySelectorAll("div");
  expect(divs[0]!.innerHTML).toBe("<b>caller's</b>");
  expect(divs[1]!.innerHTML).toBe("<p>Fallback</p>");
});

test("named slots render each caller block at its t-call-slot", async () => {
  const child = comp("Card", ($self) => {
    const n = ($.template("<article><header></header><footer></footer></article>", 0)() as Element);
    $.slot(n.firstChild!, null, $self, "header", null, null);
    $.slot(n.lastChild!, null, $self, "footer", null, null);
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Card", {}, null, {
      header: { render: () => ($.template("<h1>Title</h1>", 0)() as Element) },
      footer: { render: () => ($.template("<small>end</small>", 0)() as Element) },
    });
    return n;
  }, { components: { Card: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.querySelector("header")!.innerHTML).toBe("<h1>Title</h1>");
  expect(root.querySelector("footer")!.innerHTML).toBe("<small>end</small>");
});

test("a scoped slot receives the child's scope object and rebuilds when the child passes another", async () => {
  const row = signal({ name: "first" });
  const child = comp("List", ($self) => {
    const n = ($.template("<ul></ul>", 0)() as Element);
    $.slot(n, null, $self, "item", () => row(), null);
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "List", {}, null, {
      item: {
        render: ($s1?: object) => {
          const li = ($.template("<li></li>", 0)() as Element);
          $.insert(li, () => `${$self.prefix}${($s1 as { name: string }).name}`, null);
          return li;
        },
      },
    });
    return n;
  }, { components: { List: child.C }, setup: (self) => (self.prefix = "> ") });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.querySelector("li")!.textContent).toBe("> first");
  row.set({ name: "second" });
  flushSync();
  expect(root.querySelector("li")!.textContent).toBe("> second");
});

test("a component receiving slots is not re-rendered when its parent's state changes", async () => {
  const parentState = signal(0);
  let childRenders = 0;
  let slotRenders = 0;
  const child = comp("Child", ($self) => {
    childRenders++;
    const n = ($.template("<div></div>", 0)() as Element);
    $.slot(n, null, $self, "default", null, null);
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<main><!></main>", 0)() as Element);
    $.insert(n, () => parentState(), n.firstChild);
    $.component(n, null, $self, "Child", { v: () => parentState() }, null, {
      default: {
        render: () => {
          slotRenders++;
          return ($.template("<b>slot</b>", 0)() as Element);
        },
      },
    });
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  parentState.set(1);
  flushSync();
  parentState.set(2);
  flushSync();
  expect(root.querySelector("main")!.firstChild!.textContent).toBe("2");
  expect(childRenders).toBe(1);
  expect(slotRenders).toBe(1);
});

test("a slot: the caller's data stays text (security §8)", async () => {
  const child = comp("Child", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.slot(n, null, $self, "default", null, null);
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Child", {}, null, {
      default: {
        render: () => {
          const b = ($.template("<b></b>", 0)() as Element);
          $.insert(b, () => "<img src=x onerror=alert(1)>", null);
          return b;
        },
      },
    });
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.querySelector("img")).toBeNull();
  expect(root.textContent).toBe("<img src=x onerror=alert(1)>");
});

test("component: a prop holding markup reaches the child as a plain value and renders as text (security §8)", async () => {
  const child = comp("Child", ($self) => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => $self.props.html, null);
    $.effect(() => $.attr(n, "title", $self.props.html));
    return n;
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", { html: () => "<img src=x onerror=alert(1)>" }, null, null);
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  expect(root.querySelector("img")).toBeNull();
  expect(root.querySelector("p")!.getAttribute("title")).toBe("<img src=x onerror=alert(1)>");
});

test("an error walks the owner chain: handlers in reverse order, a throwing handler replaces the error", async () => {
  const log: string[] = [];
  const child = comp("Child", () => {
    throw new Error("render failed");
  });
  const middle = comp("Middle", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", {}, null, null);
    return n;
  }, {
    components: { Child: child.C },
    setup: () => {
      onError((e) => void log.push(`middle first ${(e as Error).message}`));
      onError((e) => {
        log.push(`middle second ${(e as Error).message}`);
        throw new Error("replaced");
      });
    },
  });
  const top = comp("Top", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Middle", {}, null, null);
    return n;
  }, { components: { Middle: middle.C }, setup: () => onError((e) => void log.push(`top ${(e as Error).message}`)) });
  await mountApp(top.C, [top.t, middle.t, child.t]);
  expect(log).toEqual(["middle second render failed", "middle first replaced"]);
});

test("an ErrorBoundary switches to its fallback slot with {error, reset} and recovers on reset", async () => {
  const fail = signal(false);
  let reset!: () => void;
  const inner = comp("Inner", () => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => {
      if (fail()) throw new Error("broken");
      return "fine";
    }, null);
    return n;
  });
  const sibling = signal("s1");
  const page = comp("Page", ($self) => {
    const n = ($.template("<main><!><span></span></main>", 0)() as Element);
    $.component(n, n.firstChild, $self, "ErrorBoundary", {}, null, {
      default: {
        render: () => {
          const d = ($.template("<div></div>", 0)() as Element);
          $.component(d, null, $self, "Inner", {}, null, null);
          return d;
        },
      },
      fallback: {
        render: ($s0?: object) => {
          const scope = $s0 as { error: Error; reset: () => void };
          reset = scope.reset;
          const e = ($.template('<em role="alert"></em>', 0)() as Element);
          $.insert(e, () => scope.error.message, null);
          return e;
        },
      },
    });
    $.insert(n.lastChild!, () => sibling(), null);
    return n;
  }, { components: { ErrorBoundary, Inner: inner.C } });
  const { root } = await mountApp(page.C, [page.t, inner.t]);
  expect(root.querySelector("p")!.textContent).toBe("fine");
  fail.set(true);
  flushSync();
  expect(root.querySelector("p")).toBeNull();
  expect(root.querySelector("em")!.textContent).toBe("broken");
  sibling.set("s2"); // the rest of the page stays live
  flushSync();
  expect(root.querySelector("span")!.textContent).toBe("s2");
  fail.set(false);
  reset();
  flushSync();
  expect(root.querySelector("em")).toBeNull();
  expect(root.querySelector("p")!.textContent).toBe("fine");
});

test("an ErrorBoundary without a fallback slot shows a recoverable panel with a Retry button", async () => {
  let broken = true;
  const inner = comp("Inner", () => {
    if (broken) throw new Error("setup data missing");
    return ($.template("<p>ok</p>", 0)() as Element);
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "ErrorBoundary", {}, null, {
      default: {
        render: () => {
          const d = ($.template("<div></div>", 0)() as Element);
          $.component(d, null, $self, "Inner", {}, null, null);
          return d;
        },
      },
    });
    return n;
  }, { components: { ErrorBoundary, Inner: inner.C } });
  const { root } = await mountApp(page.C, [page.t, inner.t]);
  const panel = root.querySelector('[role="alert"]')!;
  expect(panel.textContent).toContain("could not be displayed");
  broken = false;
  (panel.querySelector("button") as HTMLButtonElement).click();
  expect(root.querySelector("p")!.textContent).toBe("ok");
});

test("an unhandled error shows the root crash panel and is reported; the app is not silently destroyed", async () => {
  const errors: unknown[] = [];
  const fail = signal(false);
  const { C, t } = comp("Root", () => {
    const n = ($.template('<p class="content"></p>', 0)() as Element);
    $.insert(n, () => {
      if (fail()) throw new Error("unhandled");
      return "ok";
    }, null);
    return n;
  });
  const { root } = await mountApp(C, [t], { errors });
  fail.set(true);
  flushSync();
  const panel = root.querySelector('[role="alert"]')!;
  expect(panel).not.toBeNull();
  expect(panel.textContent).toBe("The application stopped after an error. Reload the page to continue.");
  expect(root.querySelector(".content")).toBeNull();
  expect((errors[0] as Error).message).toBe("unhandled");
});

test("the crash panel shows the error's name and message only in development mode (security review L5)", async () => {
  const errors: unknown[] = [];
  const fail = signal(false);
  const { C, t } = comp("Root", () => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => {
      if (fail()) throw new Error("template web.Secret failed");
      return "ok";
    }, null);
    return n;
  });
  const { root } = await mountApp(C, [t], { errors, dev: true });
  fail.set(true);
  flushSync();
  expect(root.querySelector(".o_erp_crash pre")!.textContent).toBe("Error: template web.Secret failed");
  const prod = comp("Prod", () => {
    throw new Error("template web.Secret failed");
  });
  const root2 = target();
  await expect(mount(prod.C, root2, { templates: [prod.t], onError: (e) => errors.push(e) })).rejects.toThrow(
    "template web.Secret failed",
  );
  expect(root2.textContent).not.toContain("web.Secret");
  expect(root2.querySelector(".o_erp_crash pre")).toBeNull();
});

test("a Pending boundary shows its fallback until every onWillStart inside resolved, then commits once", async () => {
  let resolve!: () => void;
  const log: string[] = [];
  const slow = comp("Slow", () => ($.template("<b>loaded</b>", 0)() as Element), {
    setup: () => {
      onWillStart(() => new Promise<void>((r) => (resolve = r)));
      onMounted(() => void log.push(`slow mounted ${document.querySelector("b") !== null}`));
    },
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Pending", {}, null, {
      default: {
        render: () => {
          const d = ($.template("<div></div>", 0)() as Element);
          $.component(d, null, $self, "Slow", {}, null, null);
          return d;
        },
      },
      fallback: { render: () => ($.template("<i>loading</i>", 0)() as Element) },
    });
    return n;
  }, { components: { Pending, Slow: slow.C }, setup: () => onMounted(() => void log.push("page mounted")) });
  const { root } = await mountApp(page.C, [page.t, slow.t]);
  expect(root.querySelector("main")!.innerHTML).toBe("<i>loading</i>");
  expect(log).toEqual(["page mounted"]);
  resolve();
  await settle();
  expect(root.querySelector("main")!.textContent).toBe("loaded");
  expect(log).toEqual(["page mounted", "slow mounted true"]);
});

test("a rejected onWillStart goes to the component's error chain", async () => {
  const log: string[] = [];
  const failing = comp("Failing", () => ($.template("<b></b>", 0)() as Element), {
    setup: () => onWillStart(() => Promise.reject(new Error("load failed"))),
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Failing", {}, null, null);
    return n;
  }, { components: { Failing: failing.C }, setup: () => onError((e) => void log.push((e as Error).message)) });
  const { root } = await mountApp(page.C, [page.t, failing.t]);
  expect(log).toEqual(["load failed"]);
  expect(root.querySelector("main")!.innerHTML).toBe("");
});

test("unmount before an async onWillStart settles leaves the root empty, and the element mounts again (review H1)", async () => {
  let resolve!: () => void;
  const slow = comp("Slow", () => ($.template("<b>late</b>", 0)() as Element), {
    setup: () => onWillStart(() => new Promise<void>((r) => (resolve = r))),
  });
  const root = target();
  const outcome = mount(slow.C, root, { templates: [slow.t] }).then(
    () => "resolved",
    (e: unknown) => e,
  );
  unmount(root);
  resolve();
  await settle();
  const error = await outcome;
  expect(error).toBeInstanceOf(UIError);
  expect((error as UIError).message).toBe("unmount: the app was unmounted before it finished mounting");
  expect(root.childNodes).toHaveLength(0);
  const again = comp("Again", () => ($.template("<i>again</i>", 0)() as Element));
  await mount(again.C, root, { templates: [again.t] });
  expect(root.innerHTML).toBe("<i>again</i>");
});

test("an error thrown by onWillUnmount during unmount is reported and paints nothing on the released root (review M1)", async () => {
  const errors: unknown[] = [];
  const { C, t } = comp("Root", () => ($.template("<p>x</p>", 0)() as Element), {
    setup: () =>
      onWillUnmount(() => {
        throw new Error("cleanup failed");
      }),
  });
  const { root } = await mountApp(C, [t], { errors });
  unmount(root);
  expect(root.innerHTML).toBe("");
  expect(errors.map((e) => (e as Error).message)).toEqual(["cleanup failed"]);
});

test("a mount that fails before the root component exists rejects and leaves the element free (review M2)", async () => {
  const { C, t } = comp("Root", () => ($.template("<p>ok</p>", 0)() as Element));
  const root = target();
  const props = Object.defineProperty({}, "boom", {
    enumerable: true,
    get() {
      throw new Error("props getter failed");
    },
  });
  await expect(mount(C, root, { templates: [t], props })).rejects.toThrow("props getter failed");
  expect(root.innerHTML).toBe("");
  await mount(C, root, { templates: [t] });
  expect(root.innerHTML).toBe("<p>ok</p>");
});

test("mounting an element twice rejects, never throws synchronously, and names the fix (review M2)", async () => {
  const { C, t } = comp("Root", () => null);
  const root = target();
  await mount(C, root, { templates: [t] });
  let second: Promise<unknown> | undefined;
  expect(() => {
    second = mount(C, root, { templates: [t] });
  }).not.toThrow();
  await expect(second).rejects.toThrow(
    new UIError("mount: an app is already mounted on this element; unmount it first", "usage"),
  );
});

test("after a mount that crashed, the element stays taken until unmount, which also removes the crash panel", async () => {
  const errors: unknown[] = [];
  const bad = comp("Bad", () => {
    throw new Error("render failed");
  });
  const root = target();
  await expect(mount(bad.C, root, { templates: [bad.t], onError: (e) => errors.push(e) })).rejects.toThrow(
    "render failed",
  );
  expect(root.querySelector(".o_erp_crash")).not.toBeNull();
  await expect(mount(bad.C, root, { templates: [bad.t] })).rejects.toThrow(
    "mount: an app is already mounted on this element; unmount it first",
  );
  unmount(root);
  expect(root.innerHTML).toBe("");
  const good = comp("Good", () => ($.template("<p>ok</p>", 0)() as Element));
  await mount(good.C, root, { templates: [good.t] });
  expect(root.innerHTML).toBe("<p>ok</p>");
  expect(errors.map((e) => (e as Error).message)).toEqual(["render failed"]);
});

test("a crash before the mount resolved only rejects the mount promise; later unhandled errors are reported (review L2)", async () => {
  const g = globalThis as { reportError?: (e: unknown) => void };
  const saved = g.reportError;
  const reported: unknown[] = [];
  g.reportError = (e) => reported.push(e);
  try {
    const bad = comp("Bad", () => {
      throw new Error("render failed");
    });
    await expect(mount(bad.C, target(), { templates: [bad.t] })).rejects.toThrow("render failed");
    expect(reported).toEqual([]);
    const fail = signal(false);
    const later = comp("Later", () => {
      const n = ($.template("<p></p>", 0)() as Element);
      $.insert(n, () => {
        if (fail()) throw new Error("later failure");
        return "ok";
      }, null);
      return n;
    });
    await mount(later.C, target(), { templates: [later.t] });
    fail.set(true);
    flushSync();
    expect(reported.map((e) => (e as Error).message)).toEqual(["later failure"]);
  } finally {
    g.reportError = saved;
  }
});

test("an error thrown by setup() starts at the parent's handlers, not at the failing component's own (review L10)", async () => {
  const log: string[] = [];
  const child = comp("Child", () => null, {
    setup: () => {
      onError(() => void log.push("child handler"));
      throw new Error("setup failed");
    },
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", {}, null, null);
    return n;
  }, { components: { Child: child.C }, setup: () => onError((e) => void log.push(`parent ${(e as Error).message}`)) });
  await mountApp(parent.C, [parent.t, child.t]);
  expect(log).toEqual(["parent setup failed"]);
});

test("when a later onWillStart throws, the promises of earlier ones get a rejection handler (review L3)", async () => {
  const handlers: unknown[] = [];
  const thenable = {
    then(_resolve: unknown, reject: unknown) {
      handlers.push(reject);
      return thenable;
    },
  };
  const errors: unknown[] = [];
  const c = comp("C", () => null, {
    setup: () => {
      onWillStart(() => thenable);
      onWillStart(() => {
        throw new Error("second hook failed");
      });
    },
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "C", {}, null, null);
    return n;
  }, { components: { C: c.C }, setup: () => onError((e) => void errors.push(e)) });
  await mountApp(page.C, [page.t, c.t]);
  expect(handlers).toHaveLength(1);
  expect(typeof handlers[0]).toBe("function");
  expect(errors.map((e) => (e as Error).message)).toEqual(["second hook failed"]);
});

test("an ErrorBoundary never mounts content that failed while it was built, and reports a second error (review M4)", async () => {
  const errors: unknown[] = [];
  const log: string[] = [];
  let broken = true;
  let reset!: () => void;
  const good = comp("Good", () => ($.template("<b>good</b>", 0)() as Element), {
    setup: () => onMounted(() => void log.push("good mounted")),
  });
  const bad1 = comp("Bad1", () => {
    if (broken) throw new Error("first");
    return ($.template("<i>1</i>", 0)() as Element);
  });
  const bad2 = comp("Bad2", () => {
    if (broken) throw new Error("second");
    return ($.template("<i>2</i>", 0)() as Element);
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "ErrorBoundary", {}, null, {
      default: {
        render: () => {
          const d = ($.template("<div></div>", 0)() as Element);
          $.component(d, null, $self, "Good", {}, null, null);
          $.component(d, null, $self, "Bad1", {}, null, null);
          $.component(d, null, $self, "Bad2", {}, null, null);
          return d;
        },
      },
      fallback: {
        render: ($s0?: object) => {
          const scope = $s0 as { error: Error; reset: () => void };
          reset = scope.reset;
          const e = ($.template("<em></em>", 0)() as Element);
          $.insert(e, () => scope.error.message, null);
          return e;
        },
      },
    });
    return n;
  }, { components: { ErrorBoundary, Good: good.C, Bad1: bad1.C, Bad2: bad2.C } });
  const { root } = await mountApp(page.C, [page.t, good.t, bad1.t, bad2.t], { errors });
  expect(root.querySelector("em")!.textContent).toBe("first");
  expect(errors.map((e) => (e as Error).message)).toEqual(["second"]);
  reset(); // after the first mount: the rebuild fails again
  expect(root.querySelector("em")!.textContent).toBe("first");
  expect(log).toEqual([]);
  expect(errors.map((e) => (e as Error).message)).toEqual(["second", "second"]);
  broken = false;
  reset();
  expect(log).toEqual(["good mounted"]);
  expect(root.querySelector("div")!.textContent).toBe("good12");
});

test("useEffect that reads no signal keeps its cleanup until the component is destroyed (review H2)", async () => {
  const log: string[] = [];
  const { C, t } = comp("Root", () => null, {
    setup: () =>
      useEffect(() => {
        log.push("listen");
        return () => log.push("unlisten");
      }),
  });
  const { root } = await mountApp(C, [t]);
  expect(log).toEqual(["listen"]);
  unmount(root);
  expect(log).toEqual(["listen", "unlisten"]);
});

test("signals written in onMounted and onWillUnmount update the bindings that read them", async () => {
  const status = signal("init");
  const show = signal(true);
  const child = comp("Child", () => null, {
    setup: () => {
      onMounted(() => status.set("mounted"));
      onWillUnmount(() => status.set("unmounting"));
    },
  });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div><p></p><!></div>", 0)() as Element);
    $.insert(n.firstChild!, () => status(), null);
    $.cond(n, n.lastChild, () => (show() ? 0 : -1), [
      () => {
        const s = ($.template("<span></span>", 0)() as Element);
        $.component(s, null, $self, "Child", {}, null, null);
        return s;
      },
    ]);
    return n;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  flushSync();
  expect(root.querySelector("p")!.textContent).toBe("mounted");
  show.set(false);
  flushSync();
  expect(root.querySelector("p")!.textContent).toBe("unmounting");
});

test("unmount called from an event handler: handlers of the disposed app do not run for the rest of the event", async () => {
  const log: string[] = [];
  let root!: HTMLElement;
  const { C, t } = comp("Root", () => {
    const n = ($.template("<div><button></button></div>", 0)() as Element);
    const button = n.firstChild as Element;
    $.on(button, "click", () => log.push("inner direct"), 0);
    $.on(button, "click", () => {
      log.push("inner delegated");
      unmount(root);
    }, SYNTHETIC);
    $.on(n, "click", () => log.push("outer delegated"), SYNTHETIC);
    return n;
  });
  ({ root } = await mountApp(C, [t]));
  root.querySelector("button")!.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  expect(log).toEqual(["inner direct", "inner delegated"]);
  expect(root.innerHTML).toBe("");
});

test("a t-props spread never sets props.slots, with or without slot content (security review L2)", async () => {
  let props: Record<string, unknown> = {};
  const child = comp("Child", () => null, { setup: (self) => (props = self.props) });
  const parent = comp("Parent", ($self) => {
    const n = ($.template("<div></div>", 0)() as Element);
    $.component(n, null, $self, "Child", {}, () => ({ slots: { default: { render: () => null } }, a: 1 }), null);
    return n;
  }, { components: { Child: child.C } });
  await mountApp(parent.C, [parent.t, child.t]);
  expect(props.a).toBe(1);
  expect("slots" in props).toBe(false);
  expect(Object.keys(props)).toEqual(["a"]);
});

test("a Pending inside a Pending shows each fallback and commits each when its own content is ready", async () => {
  let resolveOuter!: () => void;
  let resolveInner!: () => void;
  const outerSlow = comp("OuterSlow", () => ($.template("<b>outer</b>", 0)() as Element), {
    setup: () => onWillStart(() => new Promise<void>((r) => (resolveOuter = r))),
  });
  const innerSlow = comp("InnerSlow", () => ($.template("<i>inner</i>", 0)() as Element), {
    setup: () => onWillStart(() => new Promise<void>((r) => (resolveInner = r))),
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main></main>", 0)() as Element);
    $.component(n, null, $self, "Pending", {}, null, {
      default: {
        render: () => {
          const d = ($.template("<div><!></div>", 0)() as Element);
          $.component(d, d.firstChild, $self, "OuterSlow", {}, null, null);
          $.component(d, null, $self, "Pending", {}, null, {
            default: {
              render: () => {
                const s = ($.template("<section></section>", 0)() as Element);
                $.component(s, null, $self, "InnerSlow", {}, null, null);
                return s;
              },
            },
            fallback: { render: () => ($.template("<small>inner loading</small>", 0)() as Element) },
          });
          return d;
        },
      },
      fallback: { render: () => ($.template("<small>outer loading</small>", 0)() as Element) },
    });
    return n;
  }, { components: { Pending, OuterSlow: outerSlow.C, InnerSlow: innerSlow.C } });
  const { root } = await mountApp(page.C, [page.t, outerSlow.t, innerSlow.t]);
  const main = root.querySelector("main")!;
  expect(main.textContent).toBe("outer loading");
  resolveOuter();
  await settle();
  expect(main.textContent).toBe("outerinner loading");
  resolveInner();
  await settle();
  expect(main.textContent).toBe("outerinner");
});

test("a child created after the first mount at a null marker appears at the end of its parent once it resolves", async () => {
  let resolve!: () => void;
  const show = signal(false);
  const late = comp("Late", () => ($.template("<b>late</b>", 0)() as Element), {
    setup: () => onWillStart(() => new Promise<void>((r) => (resolve = r))),
  });
  const page = comp("Page", ($self) => {
    const n = ($.template("<main><i>first</i><!></main>", 0)() as Element);
    $.cond(n, n.lastChild, () => (show() ? 0 : -1), [
      () => {
        const s = ($.template("<section><u>before</u></section>", 0)() as Element);
        $.component(s, null, $self, "Late", {}, null, null);
        return s;
      },
    ]);
    return n;
  }, { components: { Late: late.C } });
  const { root } = await mountApp(page.C, [page.t, late.t]);
  show.set(true);
  flushSync();
  expect(root.querySelector("section")!.innerHTML).toBe("<u>before</u>");
  resolve();
  await settle();
  expect(root.querySelector("section")!.innerHTML).toBe("<u>before</u><b>late</b>");
});

test("a component and a slot at a null marker under a fragment keep their place once the fragment is inserted", async () => {
  const label = signal("a");
  const child = comp("Child", ($self) => {
    const f = $.template("<i>child</i>", 1)();
    $.slot(f, null, $self, "default", null, null);
    return f;
  });
  const parent = comp("Parent", ($self) => {
    const f = $.template("<p>start</p>", 1)();
    $.component(f, null, $self, "Child", {}, null, {
      default: {
        render: () => {
          const b = ($.template("<b></b>", 0)() as Element);
          $.insert(b, () => label(), null);
          return b;
        },
      },
    });
    return f;
  }, { components: { Child: child.C } });
  const { root } = await mountApp(parent.C, [parent.t, child.t]);
  root.appendChild(document.createElement("hr")); // content added after the app
  expect(root.textContent).toBe("startchilda");
  label.set("b");
  flushSync();
  expect(root.innerHTML).toBe("<p>start</p><i>child</i><b>b</b><hr>");
});

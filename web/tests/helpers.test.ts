/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The DOM helpers of the ABI, written as the compiler writes them.

import { expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import { Markup, UIError, UnsafeAttributeError, flushSync, htmlEscape, isMarkup, markup, onError, signal } from "../src/index.js";
import { type Self, comp, mountApp } from "./harness.js";

/** Mounts one component whose render function is `render` and returns its root element. */
async function view(render: (self: Self) => Node | null, setup?: (self: Self) => void, errors?: unknown[]) {
  const { C, t } = comp("T", render, { setup });
  return (await mountApp(C, [t], { errors })).root;
}

test("template clones one root element, or a fragment of roots, from the parsed HTML", async () => {
  const one = $.template('<p class="a">x<b>y</b></p>', 0);
  const many = $.template("<i>1</i><i>2</i>", 1);
  const a = one() as Element;
  const b = one() as Element;
  expect(a).not.toBe(b);
  expect(a.outerHTML).toBe('<p class="a">x<b>y</b></p>');
  expect(a.ownerDocument).toBe(document);
  const f = many();
  expect(f.nodeType).toBe(11);
  expect(f.childNodes).toHaveLength(2);
  expect(() => $.template("<i></i><i></i>", 0)).toThrow(UIError);
  expect(() => $.template("<p><script>alert(1)</script></p>", 0)).toThrow(/script/);
});

test("insert renders text, updates it in place, and renders null and undefined as nothing", async () => {
  const s = signal<unknown>("hi");
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => s(), null);
    return n;
  });
  const p = root.querySelector("p")!;
  const text = p.firstChild;
  expect(p.textContent).toBe("hi");
  s.set("there");
  flushSync();
  expect(p.firstChild).toBe(text);
  expect(p.textContent).toBe("there");
  s.set(null);
  flushSync();
  expect(p.childNodes).toHaveLength(0);
  s.set(true);
  flushSync();
  expect(p.textContent).toBe("true");
  s.set(false);
  flushSync();
  expect(p.textContent).toBe("false");
  s.set(0);
  flushSync();
  expect(p.textContent).toBe("0");
  s.set({ toString: () => "lazy translation" });
  flushSync();
  expect(p.textContent).toBe("lazy translation");
  s.set(undefined);
  flushSync();
  expect(p.textContent).toBe("");
});

/// (partial: runtime half, t-out escapes text and renders only Markup as HTML)
test("insert renders a Markup value as HTML and every other string as text", async () => {
  const s = signal<unknown>(markup("<b>bold</b>"));
  const root = await view(() => {
    const n = ($.template("<p><!>end</p>", 0)() as Element);
    $.insert(n, () => s(), n.firstChild);
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.querySelector("b")?.textContent).toBe("bold");
  s.set("<b>bold</b>");
  flushSync();
  expect(p.querySelector("b")).toBeNull();
  expect(p.textContent).toBe("<b>bold</b>end");
  s.set(markup`<i>${"<script>x</script>"}</i>`);
  flushSync();
  expect(p.querySelector("i")?.textContent).toBe("<script>x</script>");
  expect(p.querySelector("script")).toBeNull();
});

test("insert: a string holding markup or script stays text (security §8)", async () => {
  const payloads = ['<img src=x onerror="alert(1)">', "</script><script>alert(1)</script>", "javascript:alert(1)"];
  for (const payload of payloads) {
    const root = await view(() => {
      const n = ($.template("<p></p>", 0)() as Element);
      $.insert(n, () => payload, null);
      return n;
    });
    const p = root.querySelector("p")!;
    expect(p.childNodes).toHaveLength(1);
    expect(p.firstChild!.nodeType).toBe(3);
    expect(p.textContent).toBe(payload);
    expect(p.querySelector("img, script")).toBeNull();
  }
});

test("out renders its fallback for null or undefined and drops it when a value arrives", async () => {
  const s = signal<unknown>(undefined);
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.out(n, () => s(), null, () => ($.template("<em>none</em>", 0)() as Element));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.innerHTML).toBe("<em>none</em>");
  s.set("value");
  flushSync();
  expect(p.innerHTML).toBe("value");
  s.set(null);
  flushSync();
  expect(p.innerHTML).toBe("<em>none</em>");
});

test("out: a string holding markup stays text; only a Markup instance is HTML (security §8)", async () => {
  const s = signal<unknown>("<img src=x onerror=alert(1)>");
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.out(n, () => s(), null, () => null);
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.querySelector("img")).toBeNull();
  expect(p.textContent).toBe("<img src=x onerror=alert(1)>");
  s.set(new String("<img src=x>"));
  flushSync();
  expect(p.querySelector("img")).toBeNull();
});

/// (partial: runtime half, attribute values, boolean attributes and t-attf interpolation)
test("attr removes on false, null and undefined, sets an empty value for true and the string otherwise", async () => {
  const s = signal<unknown>("a");
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.effect(() => $.attr(n, "title", s()));
    $.effect(() => $.attr(n, "data-x", $.interp("id-", s(), "-", null, "/", undefined)));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.getAttribute("title")).toBe("a");
  expect(p.getAttribute("data-x")).toBe("id-a-/");
  for (const [v, want] of [
    [true, ""],
    [false, null],
    [null, null],
    [undefined, null],
    [0, "0"],
    ["x", "x"],
  ] as const) {
    s.set(v);
    flushSync();
    expect(p.getAttribute("title")).toBe(want);
  }
});

test("attr never evaluates a value and refuses event handlers and script URLs (security §8)", async () => {
  const errors: unknown[] = [];
  const root = await view(() => {
    const n = ($.template("<a></a>", 0)() as Element);
    $.effect(() => $.attr(n, "title", '"><img src=x onerror=alert(1)>'));
    return n;
  });
  const a = root.querySelector("a")!;
  expect(a.getAttribute("title")).toBe('"><img src=x onerror=alert(1)>');
  expect(a.children).toHaveLength(0);
  const el = document.createElement("a");
  expect(() => $.attr(el, "onclick", "alert(1)")).toThrow(UnsafeAttributeError);
  expect(() => $.attr(el, "ONMOUSEOVER", "alert(1)")).toThrow(UnsafeAttributeError);
  for (const url of ["javascript:alert(1)", " JavaScript:alert(1)", "java\tscript:alert(1)", "vbscript:x", "data:text/html,<script>alert(1)</script>", "data:image/svg+xml,<svg onload=alert(1)>"]) {
    expect(() => $.attr(el, "href", url)).toThrow(UnsafeAttributeError);
  }
  expect(() => $.attr(el, "srcdoc", "<p>x</p>")).toThrow(UnsafeAttributeError);
  $.attr(el, "href", "/erp/action-1");
  $.attr(el, "src", "data:image/png;base64,AAAA");
  expect(el.getAttribute("href")).toBe("/erp/action-1");
  expect(el.getAttribute("src")).toBe("data:image/png;base64,AAAA");
  expect(errors).toEqual([]);
});

test("cls merges with the static class and never removes a static class", async () => {
  const s = signal<unknown>("a b");
  const root = await view(() => {
    const n = ($.template('<p class="o_static a"></p>', 0)() as Element);
    $.effect(() => $.cls(n, s()));
    return n;
  });
  const p = root.querySelector("p")!;
  expect([...p.classList]).toEqual(["o_static", "a", "b"]);
  s.set({ c: true, "d e": 1, b: false });
  flushSync();
  expect([...p.classList].sort()).toEqual(["a", "c", "d", "e", "o_static"]); // BUG-068: static "a" stays
  s.set({ o_static: false, a: false });
  flushSync();
  expect([...p.classList].sort()).toEqual(["a", "o_static"]);
  s.set(false); // BUG-067: no class named "false"
  flushSync();
  expect(p.classList.contains("false")).toBe(false);
  s.set(7);
  flushSync();
  expect(p.classList.contains("7")).toBe(true);
  expect(() => $.cls(document.createElement("p"), true)).toThrow(UIError);
  expect(() => $.cls(document.createElement("p"), ["a"])).toThrow(UIError);
});

test("cls: a value holding markup only ever becomes class tokens (security §8)", async () => {
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.effect(() => $.cls(n, '"><img src=x onerror=alert(1)>'));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.children).toHaveLength(0);
  expect([...p.classList]).toEqual(['"><img', "src=x", "onerror=alert(1)>"]);
});

test("style merges with the static style, restores static values and drops empty style", async () => {
  const s = signal<unknown>({ color: "red", marginTop: 0 });
  const root = await view(() => {
    const n = ($.template('<p style="color: blue; width: 1px"></p>', 0)() as Element);
    $.effect(() => $.style(n, s()));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.style.color).toBe("red");
  expect(p.style.marginTop).toBe("0px");
  expect(p.style.width).toBe("1px");
  s.set("background: url('a;b.png'); height: 2px !important");
  flushSync();
  expect(p.style.color).toBe("blue");
  expect(p.style.marginTop).toBe("");
  expect(p.style.getPropertyPriority("height")).toBe("important");
  expect(p.style.backgroundImage).toContain("a;b.png");
  const bare = document.createElement("p");
  $.style(bare, { color: "red" });
  $.style(bare, null);
  expect(bare.hasAttribute("style")).toBe(false);
  expect(() => $.style(bare, 3)).toThrow(UIError);
});

test("style: a value holding markup only ever becomes declarations (security §8)", async () => {
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.effect(() => $.style(n, 'color: red"><img src=x onerror=alert(1)>'));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.children).toHaveLength(0);
  expect(p.outerHTML).not.toContain("<img");
});

test("prop sets isProp's IDL properties and refuses any other name", async () => {
  const v = signal<unknown>("x");
  const c = signal<unknown>(1);
  const root = await view(() => {
    const n = ($.template("<input>", 0)() as Element);
    $.effect(() => $.prop(n, "value", v()));
    $.effect(() => $.prop(n, "checked", c()));
    $.effect(() => $.prop(n, "readonly", c()));
    return n;
  });
  const input = root.querySelector("input")!;
  expect(input.value).toBe("x");
  expect(input.checked).toBe(true);
  expect(input.readOnly).toBe(true);
  v.set(0);
  c.set(0);
  flushSync();
  expect(input.value).toBe("0");
  expect(input.checked).toBe(false);
  expect(input.readOnly).toBe(false);
  v.set(null);
  flushSync();
  expect(input.value).toBe("");
  expect(() => $.prop(document.createElement("div"), "value", "x")).toThrow(UnsafeAttributeError);
  expect(() => $.prop(document.createElement("input"), "innerHTML", "<b>")).toThrow(UnsafeAttributeError);
});

test("prop: a value holding markup is assigned as a plain value (security §8)", async () => {
  const root = await view(() => {
    const n = ($.template("<textarea></textarea>", 0)() as Element);
    $.effect(() => $.prop(n, "value", "</textarea><img src=x onerror=alert(1)>"));
    return n;
  });
  const ta = root.querySelector("textarea")!;
  expect(ta.value).toBe("</textarea><img src=x onerror=alert(1)>");
  expect(root.querySelector("img")).toBeNull();
});

test("attrs applies an object or a pair, merges class and style, and restores static attributes", async () => {
  const s = signal<unknown>({ title: "t", class: "dyn", "data-a": 1 });
  const root = await view(() => {
    const n = ($.template('<p title="static" class="s"></p>', 0)() as Element);
    $.effect(() => $.attrs(n, s()));
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.getAttribute("title")).toBe("t");
  expect([...p.classList]).toEqual(["s", "dyn"]);
  expect(p.getAttribute("data-a")).toBe("1");
  s.set(["data-b", true]);
  flushSync();
  expect(p.getAttribute("title")).toBe("static");
  expect([...p.classList]).toEqual(["s"]);
  expect(p.hasAttribute("data-a")).toBe(false);
  expect(p.getAttribute("data-b")).toBe("");
  expect(() => $.attrs(document.createElement("p"), [1, 2, 3])).toThrow(UIError);
  expect(() => $.attrs(document.createElement("p"), "a=b")).toThrow(UIError);
});

test("attrs never evaluates values and refuses bad names, handlers and script URLs (security §8)", async () => {
  const el = document.createElement("a");
  $.attrs(el, { title: "<script>alert(1)</script>" });
  expect(el.getAttribute("title")).toBe("<script>alert(1)</script>");
  expect(el.children).toHaveLength(0);
  expect(() => $.attrs(el, { 'x" onload="alert(1)': "y" })).toThrow(UnsafeAttributeError);
  expect(() => $.attrs(el, { onload: "alert(1)" })).toThrow(UnsafeAttributeError);
  expect(() => $.attrs(el, ["href", "javascript:alert(1)"])).toThrow(UnsafeAttributeError);
});

test("interp concatenates parts and never interprets them (security §8)", () => {
  expect($.interp("a", 1, null, undefined, true, "<b>")).toBe("a1true<b>");
  const el = document.createElement("p");
  $.attr(el, "title", $.interp("<img src=x onerror=", "alert(1)", ">"));
  expect(el.children).toHaveLength(0);
  expect(el.getAttribute("title")).toBe("<img src=x onerror=alert(1)>");
});

test("effect runs only functions: a string is refused, never evaluated (security §8)", async () => {
  const g = globalThis as { erpEvaluated?: boolean };
  const errors: unknown[] = [];
  const mounting = view(
    () => {
      const n = ($.template("<p></p>", 0)() as Element);
      ($.effect as (fn: unknown) => void)("globalThis.erpEvaluated = true");
      return n;
    },
    undefined,
    errors,
  );
  await expect(mounting).rejects.toThrow(UIError);
  expect(g.erpEvaluated).toBeUndefined();
  expect(errors).toHaveLength(1);
  expect(errors[0]).toBeInstanceOf(UIError);
});

test("a root position of a multi-root block keeps working after its fragment is in the document", async () => {
  const s = signal<unknown>("one");
  const root = await view(() => {
    const f = $.template("<!><i>static</i><!>", 1)();
    const m0 = f.firstChild!;
    const m1 = m0.nextSibling!.nextSibling!;
    $.insert(f, () => s(), m0);
    $.cond(f, m1, () => (s() === "two" ? 0 : -1), [() => ($.template("<b>two</b>", 0)() as Element)]);
    return f;
  });
  expect(root.textContent).toBe("onestatic");
  s.set("two");
  flushSync();
  expect(root.textContent).toBe("twostatictwo");
  expect(root.querySelector("b")).not.toBeNull();
});

test("only a Markup made by markup() or htmlEscape() renders as HTML: look-alikes and subclasses do not (security review M1)", async () => {
  const forged = Object.create(Markup.prototype) as object;
  Object.defineProperty(forged, "toString", { value: () => "<img src=x onerror=alert(1)>" });
  const real = markup("<b>ok</b>");
  // The prototype of a genuine value changed later: the HTML stored at creation is what renders.
  const swapped = markup("<i>stored</i>");
  Object.setPrototypeOf(swapped, { toString: () => "<img src=x onerror=alert(2)>" });
  const values = signal<unknown>(forged);
  const root = await view(() => {
    const n = ($.template("<p></p>", 0)() as Element);
    $.insert(n, () => values(), null);
    return n;
  });
  const p = root.querySelector("p")!;
  expect(p.querySelector("img")).toBeNull();
  expect(p.textContent).toBe("<img src=x onerror=alert(1)>");
  values.set(real);
  flushSync();
  expect(p.innerHTML).toBe("<b>ok</b>");
  values.set(swapped);
  flushSync();
  expect(p.innerHTML).toBe("<i>stored</i>");
  expect(isMarkup(forged)).toBe(false);
  expect(isMarkup(real)).toBe(true);
  const refused = new UIError("Markup values are created by markup() or htmlEscape(); `new Markup` is refused", "usage");
  expect(() => new (Markup as unknown as new (html: string) => Markup)("<b>x</b>")).toThrow(refused);
  class Sub extends (Markup as unknown as new (token: unknown, html: string) => Markup) {}
  expect(() => new Sub(undefined, "<b>x</b>")).toThrow(refused);
});

test("markup() returns a Markup as it is and refuses a value that is neither text nor a template (review L4)", () => {
  const m = markup("<b>bold</b>");
  expect(markup(m)).toBe(m);
  expect(htmlEscape(m)).toBe(m);
  expect(String(markup`<i>${"<x>"}</i>`)).toBe("<i>&lt;x&gt;</i>");
  expect(() => (markup as (v: unknown) => Markup)(42)).toThrow(
    new UIError("markup takes a string, a Markup or a template literal, got number", "value"),
  );
});

test("attr sends class and style, in any case, to the class and style appliers, never to a raw attribute (security review M3)", async () => {
  const el = document.createElement("p");
  el.setAttribute("class", "static");
  el.setAttribute("style", "margin: 1px");
  const written: string[] = [];
  const setAttribute = el.setAttribute.bind(el);
  el.setAttribute = (name: string, value: string) => {
    written.push(name);
    setAttribute(name, value);
  };
  $.attr(el, "class", "a b");
  $.attr(el, "STYLE", "color: red; background: url(x.png)");
  $.attr(el, "Class", "c");
  expect([...el.classList]).toEqual(["static", "c"]);
  expect((el as HTMLElement).style.color).toBe("red");
  expect((el as HTMLElement).style.margin).toBe("1px");
  $.attr(el, "style", null);
  expect((el as HTMLElement).style.color).toBe("");
  expect((el as HTMLElement).style.margin).toBe("1px");
  $.attrs(el, { Style: "color: blue" });
  expect((el as HTMLElement).style.color).toBe("blue");
  expect(written).toEqual([]);
  expect(() => $.attr(el, "class", true)).toThrow(UIError);
});

test("strict CSP: dynamic styles go through the CSSOM only; a static style is the template's own attribute (security review L1)", async () => {
  // jsdom applies no Content-Security-Policy, so this pins what a policy sees: the runtime never writes a
  // `style` (or `class`) attribute itself, so `style-src 'self'` without 'unsafe-inline' allows every
  // dynamic style; a static `style` written in a template arrives as an attribute of the parsed template
  // HTML and needs `style-src-attr 'unsafe-inline'`.
  const written: string[] = [];
  const original = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (this: Element, name: string, value: string) {
    written.push(name.toLowerCase());
    original.call(this, name, value);
  };
  try {
    const s = signal<unknown>({ color: "red" });
    const root = await view(() => {
      const n = ($.template('<p style="margin: 2px"></p>', 0)() as Element);
      $.effect(() => $.style(n, s()));
      $.effect(() => $.attrs(n, { style: s(), class: "k", title: "t" }));
      $.effect(() => $.attr(n, "style", s()));
      return n;
    });
    const p = root.querySelector("p")!;
    expect(p.getAttribute("style")).toContain("margin: 2px");
    s.set("color: blue");
    flushSync();
    expect((p as HTMLElement).style.color).toBe("blue");
    expect(written.filter((n) => n === "style" || n === "class")).toEqual([]);
    expect(written).toContain("title");
  } finally {
    Element.prototype.setAttribute = original;
  }
});

test("template refuses a <script> also inside a nested <template>'s content (security review L6)", () => {
  const refused = new UIError("a template holds a <script> element; templates never run scripts", "template");
  expect(() => $.template("<div><template><script>alert(1)</script></template></div>", 0)).toThrow(refused);
  expect(() => $.template("<div><template><p><template><script></script></template></p></template></div>", 0)).toThrow(
    refused,
  );
  expect(() => $.template("<div><template><p>fine</p></template></div>", 0)).not.toThrow();
});

test("a name starting with on is refused with a message that does not invent an event (review L6)", () => {
  const el = document.createElement("p");
  for (const name of ["online", "onclick", "Once"]) {
    expect(() => $.attr(el, name, "x")).toThrow(
      new UnsafeAttributeError(
        `refusing the attribute "${name}": a name starting with "on" is an event-handler attribute, which a template never sets dynamically (for an event, write t-on-<event>)`,
      ),
    );
  }
});

test("a position whose anchor code outside the runtime removed fails with an error naming that (review L1)", async () => {
  const errors: unknown[] = [];
  const show = signal(false);
  let marker!: Node;
  const root = await view(
    () => {
      const n = ($.template("<div><!></div>", 0)() as Element);
      marker = n.firstChild!;
      $.cond(n, marker, () => (show() ? 0 : -1), [() => ($.template("<b>x</b>", 0)() as Element)]);
      return n;
    },
    () => onError((e) => void errors.push(e)),
  );
  marker.parentNode!.removeChild(marker);
  show.set(true);
  flushSync();
  expect(errors).toHaveLength(1);
  expect(errors[0]).toEqual(
    new UIError("the anchor of a dynamic position was removed from the DOM by code outside the runtime", "detached"),
  );
  expect(root.querySelector("b")).toBeNull();
});

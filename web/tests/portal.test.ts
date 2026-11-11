/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Tests for Portal component, external DOM mounting, lifecycle cleanup, and error propagation.

import { describe, expect, test } from "vitest";
import * as $ from "@erpforall/elang/abi";
import {
  Component,
  Portal,
  UIError,
  flushSync,
  mount,
  onError,
  signal,
  unmount,
} from "../src/index.js";
import { mountApp, tpl } from "./harness.js";

describe("Portal component", () => {
  test(": Portal component mounting to external DOM targets with lifecycle cleanup", async () => {
    const externalTarget = document.createElement("div");
    externalTarget.id = "external-dialog-container";
    document.body.appendChild(externalTarget);

    class PortalApp extends Component {
      static override template = "test.PortalApp";
      static override components = { Portal };
    }

    const templates = [
      {
        name: "test.PortalApp",
        render: () => document.createElement("div"),
      },
    ];

    const container = document.createElement("div");
    await mount(Portal, container, {
      templates,
      props: {
        target: externalTarget,
        slots: {
          default: {
            render: () => {
              const p = document.createElement("p");
              p.className = "dialog-text";
              p.textContent = "Modal dialog";
              return p;
            },
          },
        },
      },
    });
    flushSync();

    expect(externalTarget.querySelector(".dialog-text")?.textContent).toBe("Modal dialog");

    // Unmounting portal removes elements from external target
    unmount(container);
    expect(externalTarget.querySelector(".dialog-text")).toBeNull();

    externalTarget.remove();
  });

  test(": Error propagation across portal boundaries to virtual parent chain", async () => {
    const externalTarget = document.createElement("div");
    document.body.appendChild(externalTarget);

    let caughtError: unknown = null;

    class HostComp extends Component {
      static override template = "test.HostComp";
      static override components = { Portal };
      override setup() {
        onError((err) => {
          caughtError = err;
        });
      }
    }

    const templates = [
      {
        name: "test.HostComp",
        render: ($self: any) => {
          const div = document.createElement("div");
          $.component(
            div,
            null,
            $self,
            "Portal",
            { target: () => externalTarget },
            null,
            {
              default: {
                render: () => {
                  throw new Error("Fault inside portaled component");
                },
              },
            },
          );
          return div;
        },
      },
    ];

    const container = document.createElement("div");
    await mount(HostComp, container, { templates });
    flushSync();

    // The error thrown inside portal was propagated up to the virtual parent's onError handler!
    expect(caughtError).toBeInstanceOf(Error);
    expect((caughtError as Error).message).toBe("Fault inside portaled component");

    unmount(container);
    externalTarget.remove();
  });

  test("a Portal whose target changes moves its content to the new target (R15)", async () => {
    const first = document.body.appendChild(document.createElement("section"));
    const second = document.body.appendChild(document.createElement("section"));
    second.id = "second-target";
    const where = signal<Element | string>(first);
    class Mover extends Component {
      static override template = "test.Mover";
      static override components = { Portal };
    }
    const { root } = await mountApp(Mover, [
      tpl("test.Mover", ($self) => {
        const div = document.createElement("div");
        $.component(div, null, $self, "Portal", { target: () => where() }, null, {
          default: {
            render: () => {
              const p = document.createElement("p");
              p.textContent = "moving";
              return p;
            },
          },
        });
        return div;
      }),
    ]);
    expect(first.textContent).toBe("moving");
    where.set("#second-target");
    flushSync();
    expect(first.textContent).toBe("");
    expect(second.textContent).toBe("moving");
    expect(second.querySelector("p")).not.toBeNull();
    unmount(root);
    expect(second.childNodes.length).toBe(0);
    first.remove();
    second.remove();
  });

  test("a Portal whose new target is missing fails loudly to its owner chain (R15)", async () => {
    const first = document.body.appendChild(document.createElement("section"));
    const where = signal<Element | string>(first);
    const caught: unknown[] = [];
    class Lost extends Component {
      static override template = "test.Lost";
      static override components = { Portal };
      override setup() {
        onError((e) => void caught.push(e));
      }
    }
    await mountApp(Lost, [
      tpl("test.Lost", ($self) => {
        const div = document.createElement("div");
        $.component(div, null, $self, "Portal", { target: () => where() }, null, {
          default: { render: () => document.createTextNode("x") },
        });
        return div;
      }),
    ]);
    where.set("#no-such-target");
    flushSync();
    expect(caught).toHaveLength(1);
    expect(caught[0]).toBeInstanceOf(UIError);
    expect((caught[0] as UIError).code).toBe("usage");
    expect((caught[0] as UIError).message).toBe("Portal target not found: #no-such-target");
    first.remove();
  });

  test("a Portal whose target is not a valid selector fails with a usage UIError, not a DOMException (delta N5)", async () => {
    const first = document.body.appendChild(document.createElement("section"));
    const where = signal<Element | string>(first);
    const caught: unknown[] = [];
    class Bad extends Component {
      static override template = "test.Bad";
      static override components = { Portal };
      override setup() {
        onError((e) => void caught.push(e));
      }
    }
    await mountApp(Bad, [
      tpl("test.Bad", ($self) => {
        const div = document.createElement("div");
        $.component(div, null, $self, "Portal", { target: () => where() }, null, {
          default: { render: () => document.createTextNode("x") },
        });
        return div;
      }),
    ]);
    where.set("[[not a selector");
    flushSync();
    expect(caught).toHaveLength(1);
    expect(caught[0]).toBeInstanceOf(UIError);
    expect((caught[0] as UIError).code).toBe("usage");
    expect((caught[0] as UIError).message).toBe("Portal target is not a valid selector: [[not a selector");
    expect(((caught[0] as UIError).cause as { name?: unknown }).name).toBe("SyntaxError");
    first.remove();
  });
});

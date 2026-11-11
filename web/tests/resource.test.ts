/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Tests for resource() with AbortController, latest-wins, and error routing.

import { afterEach, describe, expect, test } from "vitest";
import {
  Component,
  type Resource,
  flushSync,
  mount,
  onError,
  resource,
  signal,
  unmount,
} from "../src/index.js";
import { mountApp, settle } from "./harness.js";

/** A fetch the test settles by hand. */
interface Deferred<T> {
  promise: Promise<T>;
  resolve: (v: T) => void;
  reject: (e: unknown) => void;
}

function deferred<T>(): Deferred<T> {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const originalReportError = (globalThis as { reportError?: (e: unknown) => void }).reportError;

afterEach(() => {
  (globalThis as { reportError?: (e: unknown) => void }).reportError = originalReportError;
});

const templates = [
  {
    name: "test.ResourceComp",
    render: () => document.createElement("div"),
  },
];

describe("resource() primitive", () => {
  test("resource() tracks data, loading state, and handles abort on re-trigger", async () => {
    let abortCount = 0;
    const query = signal("initial");

    class ResourceComp extends Component {
      static override template = "test.ResourceComp";
      userResource = resource(async (abortSignal) => {
        const q = query();
        abortSignal.addEventListener("abort", () => {
          abortCount++;
        });
        await new Promise((resolve) => setTimeout(resolve, 10));
        return `Result for ${q}`;
      });
    }

    const container = document.createElement("div");
    const instance = await mount(ResourceComp, container, {
      templates,
    });

    expect(instance.userResource.loading()).toBe(true);
    expect(instance.userResource.data()).toBeUndefined();

    // Wait for first fetch to complete
    await new Promise((resolve) => setTimeout(resolve, 25));
    flushSync();

    expect(instance.userResource.loading()).toBe(false);
    expect(instance.userResource.data()).toBe("Result for initial");

    // Automatically refetches when the signal changes, without a refetch() call. The first fetch had
    // finished, so nothing is aborted (review round 3, R21).
    query.set("second");
    flushSync();
    expect(instance.userResource.loading()).toBe(true);
    expect(abortCount).toBe(0);

    // An imperative refetch() while the second fetch is in flight aborts it.
    const again = instance.userResource.refetch();
    expect(instance.userResource.loading()).toBe(true);
    expect(abortCount).toBe(1);

    await again;
    flushSync();

    expect(instance.userResource.loading()).toBe(false);
    expect(instance.userResource.data()).toBe("Result for second");
    expect(abortCount).toBe(1);

    unmount(container);
  });

  test("resource() enforces latest-wins when responses resolve out of order", async () => {
    let seq = 0;
    const responses: Array<{ resolve: (v: string) => void }> = [];

    const res = resource(
      async () => {
        const mySeq = ++seq;
        return new Promise<string>((resolve) => {
          responses[mySeq] = { resolve };
        });
      },
      { lazy: true },
    );

    // Call 1
    res.refetch();
    // Call 2
    res.refetch();

    // Call 2 resolves first
    responses[2]!.resolve("Second");
    await new Promise((resolve) => setTimeout(resolve, 10));
    flushSync();
    expect(res.data()).toBe("Second");

    // Call 1 resolves later: MUST NOT overwrite Call 2 result!
    responses[1]!.resolve("First");
    await new Promise((resolve) => setTimeout(resolve, 10));
    flushSync();
    expect(res.data()).toBe("Second");
  });

  test("abort() of an in-flight fetch aborts its signal, clears loading and keeps the data (R9, R21)", async () => {
    const d = deferred<string>();
    let signalSeen: AbortSignal | undefined;
    const res = resource(
      (abortSignal) => {
        signalSeen = abortSignal;
        return d.promise;
      },
      { lazy: true, initialValue: "before" },
    );
    const done = res.refetch();
    expect(res.loading()).toBe(true);
    res.abort();
    expect(signalSeen!.aborted).toBe(true);
    expect(res.loading()).toBe(false);
    d.resolve("late");
    await done;
    expect(res.data()).toBe("before");
    expect(res.loading()).toBe(false);
  });

  test("unmounting the owner aborts an in-flight fetch (R21)", async () => {
    let signalSeen: AbortSignal | undefined;
    const d = deferred<string>();
    class Owning extends Component {
      static override template = "test.ResourceComp";
      res = resource((abortSignal) => {
        signalSeen = abortSignal;
        return d.promise;
      });
    }
    const container = document.createElement("div");
    await mount(Owning, container, { templates });
    expect(signalSeen!.aborted).toBe(false);
    unmount(container);
    expect(signalSeen!.aborted).toBe(true);
  });

  test("refetch() rejects with the error of its fetch and sets error() (R10)", async () => {
    const failure = new Error("fetch failed");
    const res = resource<string>(() => Promise.reject(failure), { lazy: true });
    await expect(res.refetch()).rejects.toBe(failure);
    expect(res.error()).toBe(failure);
    expect(res.loading()).toBe(false);
  });

  test("a failing fetch of a resource outside a component with no onError is reported, not dropped (R10)", async () => {
    const reported: unknown[] = [];
    (globalThis as { reportError?: (e: unknown) => void }).reportError = (e) => void reported.push(e);
    const failure = new Error("no owner");
    const res = resource<string>(() => Promise.reject(failure));
    await settle();
    expect(res.error()).toBe(failure);
    expect(reported).toEqual([failure]);
  });

  test("a failing fetch goes to the owner's onError chain (R21)", async () => {
    const caught: unknown[] = [];
    const failure = new Error("owned failure");
    class Owning extends Component {
      static override template = "test.ResourceComp";
      res!: Resource<string>;
      override setup() {
        onError((e) => void caught.push(e));
        this.res = resource<string>(() => Promise.reject(failure));
      }
    }
    await mountApp(Owning, templates);
    await settle();
    expect(caught).toEqual([failure]);
  });

  test("an onError option that throws hands its error to the owner's chain (R10)", async () => {
    const caught: unknown[] = [];
    const handlerFailure = new Error("handler failed");
    class Owning extends Component {
      static override template = "test.ResourceComp";
      res!: Resource<string>;
      override setup() {
        onError((e) => void caught.push(e));
        this.res = resource<string>(() => Promise.reject(new Error("fetch failed")), {
          onError: () => {
            throw handlerFailure;
          },
        });
      }
    }
    await mountApp(Owning, templates);
    await settle();
    expect(caught).toEqual([handlerFailure]);
  });

  test("a lazy resource tracks the fetcher's reads once refetch() started it (R11)", async () => {
    const query = signal("a");
    const fetched: string[] = [];
    class Lazy extends Component {
      static override template = "test.ResourceComp";
      res = resource(
        async () => {
          const q = query();
          fetched.push(q);
          return q;
        },
        { lazy: true },
      );
    }
    const { instance } = await mountApp(Lazy, templates);
    await settle();
    expect(fetched).toEqual([]);
    await instance.res.refetch();
    expect(fetched).toEqual(["a"]);
    query.set("b");
    flushSync();
    await settle();
    expect(fetched).toEqual(["a", "b"]);
    expect(instance.res.data()).toBe("b");
  });
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment node
import { afterEach, expect, test, vi } from "vitest";
import { waitForReadiness } from "../scripts/browser-readiness.mjs";

afterEach(() => vi.useRealTimers());

test("readiness observes the same predicate after a navigation context transition", async () => {
  vi.useFakeTimers();
  const navigation = new Error("Inspected target navigated or closed");
  const expressions: string[] = [];
  const evaluate = vi.fn(async (expression: string) => {
    expressions.push(expression);
    if (expressions.length === 1) throw navigation;
    return expressions.length > 2;
  });
  const result = waitForReadiness(evaluate, "document.readyState === 'complete'")
    .then(() => ({ ready: true, error: undefined }), error => ({ ready: false, error }));
  await vi.runAllTimersAsync();
  expect(await result).toEqual({ ready: true, error: undefined });
  expect(expressions).toEqual(Array(3).fill("document.readyState === 'complete'"));
});

test("a permanently closed target fails the existing deadline and retains the exact navigation cause", async () => {
  vi.useFakeTimers();
  const navigation = new Error("Inspected target navigated or closed");
  const diagnose = vi.fn(async () => "must not inspect a destroyed context");
  const result = waitForReadiness(async () => { throw navigation; }, "expected fixture", diagnose)
    .then(() => undefined, error => error);
  await vi.runAllTimersAsync();
  const error = await result;
  expect(error).toBeInstanceOf(Error);
  expect(error.message).toBe("UI readiness failed: expected fixture; navigation=Inspected target navigated or closed");
  expect(error.cause).toBe(navigation);
  expect(diagnose).not.toHaveBeenCalled();
  expect(vi.getTimerCount()).toBe(0);
});

test("unexpected protocol errors propagate immediately with their identity and message", async () => {
  const error = new Error("Unexpected CDP method response");
  const evaluate = vi.fn(async () => { throw error; });
  await expect(waitForReadiness(evaluate, "fixture predicate")).rejects.toBe(error);
  expect(error.name).toBe("Error");
  expect(error.message).toBe("Unexpected CDP method response");
  expect(evaluate).toHaveBeenCalledTimes(1);
});

test("a plain false predicate retains its diagnostic when the deadline expires", async () => {
  vi.useFakeTimers();
  const result = waitForReadiness(async () => false, "fixture predicate", async () => "popup=fixture-state")
    .then(() => undefined, error => error);
  await vi.runAllTimersAsync();
  const error = await result;
  expect(error).toBeInstanceOf(Error);
  expect(error.message).toBe("UI readiness failed: fixture predicate; popup=fixture-state");
  expect(vi.getTimerCount()).toBe(0);
});

test("an untyped thrown value is never mistaken for a navigation protocol error", async () => {
  const value = "Inspected target navigated or closed";
  await expect(waitForReadiness(async () => { throw value; }, "fixture predicate")).rejects.toBe(value);
});

test("a recovered context uses predicate diagnostics rather than a stale navigation cause", async () => {
  vi.useFakeTimers();
  let first = true;
  const result = waitForReadiness(async () => {
    if (first) { first = false; throw new Error("Inspected target navigated or closed"); }
    return false;
  }, "fixture predicate", async () => "popup=recovered-context")
    .then(() => undefined, error => error);
  await vi.runAllTimersAsync();
  const error = await result;
  expect(error).toBeInstanceOf(Error);
  expect(error.message).toBe("UI readiness failed: fixture predicate; popup=recovered-context");
  expect(error.cause).toBeUndefined();
});

test("a programming error with the same text is not classified as a navigation response", async () => {
  const error = new TypeError("Inspected target navigated or closed");
  const evaluate = vi.fn(async () => { throw error; });
  await expect(waitForReadiness(evaluate, "fixture predicate")).rejects.toBe(error);
  expect(error.name).toBe("TypeError");
  expect(error.message).toBe("Inspected target navigated or closed");
  expect(evaluate).toHaveBeenCalledTimes(1);
});


class NavigationProtocolError extends Error {
  constructor(readonly method: string, readonly detail: string) {
    super(`${method}: ${detail}`); this.name = "CdpProtocolError";
  }
}
const typedNavigation = (error: Error): boolean => error instanceof NavigationProtocolError
  && error.method === "Runtime.evaluate" && error.detail === "Inspected target navigated or closed";

test("a caller can restrict navigation recovery to its typed protocol response", async () => {
  vi.useFakeTimers();
  const navigation = new NavigationProtocolError("Runtime.evaluate", "Inspected target navigated or closed");
  const evaluate = vi.fn().mockRejectedValueOnce(navigation).mockResolvedValue(true);
  const result = waitForReadiness(evaluate, "typed predicate", async () => "", typedNavigation)
    .then(() => ({ ready: true, error: undefined }), error => ({ ready: false, error }));
  await vi.runAllTimersAsync();
  expect(await result).toEqual({ ready: true, error: undefined });
  expect(evaluate).toHaveBeenCalledTimes(2);
});

test("a strict protocol classifier refuses an ordinary error with matching navigation text", async () => {
  vi.useFakeTimers();
  const error = new Error("Inspected target navigated or closed");
  const evaluate = vi.fn(async () => { throw error; });
  const result = waitForReadiness(evaluate, "typed predicate", async () => "", typedNavigation)
    .then(() => undefined, caught => caught);
  await vi.runAllTimersAsync();
  expect(await result).toBe(error);
  expect(evaluate).toHaveBeenCalledTimes(1);
});

test("the list reload probe waits for a new document root and the long-title row", async () => {
  const { readFile } = await import("node:fs/promises");
  const { runInNewContext } = await import("node:vm");
  const source = await readFile(new URL("../scripts/browser-human-ui.mjs", import.meta.url), "utf8");
  const expression = source.match(/await wait\('([^'\n]*reloadProbe[^'\n]*)'\)/)?.[1];
  expect(expression).toBeTypeOf("string");
  if (!expression) throw new Error("The list reload readiness expression is missing");
  const row = { textContent: "Long record title department" };
  const probe = (documentElement: { dataset: { reloadProbe?: string } } | null, record = row, tableLayout = "fixed") =>
    runInNewContext(expression, {
      document: {
        documentElement,
        querySelector: (selector: string) => selector === "table[aria-label=Records]" ? { tableLayout } : record,
      },
      getComputedStyle: (element: { tableLayout: string }) => ({ tableLayout: element.tableLayout }),
    });
  expect(probe(null)).toBe(false);
  expect(probe({ dataset: { reloadProbe: "pending" } })).toBe(false);
  expect(probe({ dataset: {} }, { textContent: "Previous record" })).toBe(false);
  expect(probe({ dataset: {} }, row, "auto")).toBe(false);
  expect(probe({ dataset: {} })).toBe(true);
});

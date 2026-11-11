/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { ClientErrorReporter } from "../src/client/client-report.js";
import { ClientError } from "../src/client/transport.js";

afterEach(() => vi.restoreAllMocks());
const accepted = () => vi.fn(async () => new Response(null, { status: 204 }));
const build = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// (partial: a script failure is posted with message, path, build and stack, nothing else)
test("a script failure is reported with its message, page path, build and stack", async () => {
  const fetcher = accepted();
  const error = new TypeError("x is undefined"); error.stack = "TypeError: x is undefined\n    at render (app.js:1:2)";
  new ClientErrorReporter(fetcher, build).report("error", error);
  await vi.waitFor(() => expect(fetcher).toHaveBeenCalledOnce());
  const [path, init] = fetcher.mock.calls[0] as unknown as [string, RequestInit];
  expect(path).toBe("/erp/client-error");
  expect(init).toMatchObject({ method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" } });
  expect(JSON.parse(init.body as string)).toEqual({ kind: "error", message: "TypeError: x is undefined", path: window.location.pathname, build, stack: error.stack });
});

/// (partial: call failures are not posted, text is cut, a page stops at twenty)
test("a call failure is not reported again, long text is cut and a page stops after twenty reports", async () => {
  const fetcher = accepted();
  const reporter = new ClientErrorReporter(fetcher, build);
  reporter.report("render", new ClientError("validation", "refused", 422));
  expect(fetcher).not.toHaveBeenCalled();
  reporter.report("unhandledrejection", "y".repeat(5000));
  await vi.waitFor(() => expect(fetcher).toHaveBeenCalledOnce());
  const body = JSON.parse((fetcher.mock.calls[0] as unknown as [string, RequestInit])[1].body as string);
  expect(body).toEqual({ kind: "unhandledrejection", message: "y".repeat(1024), path: window.location.pathname, build });
  for (let i = 0; i < 30; i += 1) reporter.report("render", new Error(`again ${i}`));
  await vi.waitFor(() => expect(fetcher).toHaveBeenCalledTimes(20));
});

/// (partial: refusal and loss go to the console; a malformed build id fails at start)
test("a refused report is written to the console, never thrown, and a malformed build id fails at start", async () => {
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  new ClientErrorReporter(async () => new Response("slow down", { status: 429 }), build).report("error", new Error("boom"));
  await vi.waitFor(() => expect(warn).toHaveBeenCalledWith("Error report refused by the server: HTTP 429"));
  const lost = new TypeError("offline");
  new ClientErrorReporter(async () => { throw lost; }, build).report("error", new Error("boom"));
  await vi.waitFor(() => expect(warn).toHaveBeenCalledWith("Error report not sent:", lost));
  expect(() => new ClientErrorReporter(accepted(), "bad build!")).toThrow("Invalid application build id");
});

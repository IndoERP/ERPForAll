/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { requestLoading } from "../src/client/loading.js";
import { RpcClient } from "../src/client/transport.js";

afterEach(() => { vi.useRealTimers(); document.body.replaceChildren(); });

test("loading delays feedback, preserves focus/content, and waits for every request", () => {
  vi.useFakeTimers();
  document.body.innerHTML = '<main><button>Keep working</button></main>';
  const button = document.querySelector("button")!;
  button.focus();
  const loading = requestLoading(document.body), status = document.querySelector<HTMLElement>(".client-loading")!;
  const first = loading.begin(), second = loading.begin();
  vi.advanceTimersByTime(249); expect(status.hidden).toBe(true);
  vi.advanceTimersByTime(1); expect(status.hidden).toBe(false);
  expect(status.textContent).toBe("Loading…");
  expect(status.getAttribute("role")).toBe("status");
  expect(status.getAttribute("aria-live")).toBe("polite");
  expect(document.activeElement).toBe(button);
  first(); first(); expect(status.hidden).toBe(false);
  second(); expect(status.hidden).toBe(true);
  expect(document.querySelector("button")).toBe(button);
  const quick = loading.begin(); quick(); vi.advanceTimersByTime(300);
  expect(status.hidden).toBe(true);
  loading.dispose();
});

test("disposal cancels pending feedback and late completions cannot restore it", () => {
  vi.useFakeTimers();
  const loading = requestLoading(document.body), finish = loading.begin();
  loading.dispose(); vi.advanceTimersByTime(300); finish(); loading.begin()();
  expect(document.querySelector(".client-loading")).toBeNull();
  expect(vi.getTimerCount()).toBe(0);
});

test.each(["success", "network", "permission", "invalid-json", "abort", "session"])("RPC %s always ends loading, including overlapping requests", async kind => {
  vi.useFakeTimers();
  const loading = requestLoading(document.body), status = document.querySelector<HTMLElement>(".client-loading")!;
  const other = loading.begin();
  const abort = new AbortController();
  const rpc = new RpcClient(async () => {
    vi.advanceTimersByTime(300);
    if (kind === "network") throw new TypeError("offline");
    if (kind === "abort") { abort.abort(); throw new DOMException("aborted", "AbortError"); }
    return new Response(kind === "invalid-json" ? "invalid" : '{}', { status: kind === "permission" ? 403 : kind === "session" ? 401 : 200, headers: { "Content-Type": "application/json" } });
  }, () => {}, () => loading.begin());
  const result = rpc.call("test.note", "web_read", {}, { signal: abort.signal });
  if (kind === "success") await expect(result).resolves.toEqual({});
  else if (kind === "abort") await expect(result).rejects.toMatchObject({ name: "AbortError", message: "aborted" });
  else await expect(result).rejects.toMatchObject({
    kind: kind === "invalid-json" ? "server" : kind,
    message: kind === "network" ? "Unable to connect. Check your connection and try again."
      : kind === "session" ? "Your session has expired. Sign in again."
      : kind === "permission" ? "The request was refused. Please reload and try again."
      : "The server returned an invalid response.",
  });
  expect(status.hidden).toBe(false);
  other(); expect(status.hidden).toBe(true);
  expect(vi.getTimerCount()).toBe(0);
  loading.dispose();
});

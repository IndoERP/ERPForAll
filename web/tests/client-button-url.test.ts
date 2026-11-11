/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createUrlButton, runUrlButton, type UrlButtonEnvironment } from "../../addons/web/static/src/client/button-url.js";
import { ClientError } from "../../addons/web/static/src/client/transport.js";
const harness = () => ({ baseUrl: "https://erp.example/erp", navigate: vi.fn(async () => {}), openExternal: vi.fn((_url: string, _target: "_blank", _features: "noopener,noreferrer"): void => {}) } satisfies UrlButtonEnvironment);
afterEach(() => document.body.replaceChildren());

/// (partial: generic client URL control and validated navigation; wire and kanban unavailable)
test("a generic URL declaration renders one native button and routes same-origin navigation", async () => {
  const environment = harness(), report = vi.fn();
  const button = createUrlButton({ label: "Open", href: "/erp/action-3?view_type=list", environment, report });
  document.body.append(button); expect(button.tagName).toBe("BUTTON"); expect(button.type).toBe("button");
  button.click(); await vi.waitFor(() => expect(environment.navigate).toHaveBeenCalledWith({action: "3", view_type: "list"}));
  expect(environment.openExternal).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled();
});

/// (partial: external URL adapter uses explicit opener, reports no false failure and propagates opener errors)
test("external URLs are distinct from client routes, noopener opening is not a failure and opener errors propagate", async () => {
  // window.open returns no handle with noopener even when the tab opens, so the opener result carries no outcome.
  const env = harness(); await expect(runUrlButton("https://docs.example/help", env)).resolves.toBeUndefined();
  expect(env.openExternal).toHaveBeenCalledTimes(1);
  expect(env.openExternal).toHaveBeenCalledWith("https://docs.example/help", "_blank", "noopener,noreferrer");
  expect(env.navigate).not.toHaveBeenCalled();
  env.openExternal.mockImplementation(() => { throw new ClientError("validation", "Opener refused"); });
  await expect(runUrlButton("https://docs.example/help", env)).rejects.toMatchObject({kind: "validation", message: "Opener refused"});
});

test.each([
  ["javascript:alert(1)", "Invalid button URL: only HTTP(S) without credentials is supported."],
  ["data:text/html,hello", "Invalid button URL: only HTTP(S) without credentials is supported."],
  ["file:///etc/passwd", "Invalid button URL: only HTTP(S) without credentials is supported."],
  ["https://user:pass@example.com", "Invalid button URL: only HTTP(S) without credentials is supported."],
  ["", "Invalid button URL: expected nonempty text of at most 4096 characters."],
  ["https://[", "Invalid button URL."]
])("refuses unsafe or malformed URL %s", async (href, message) => {
  const env = harness(); await expect(runUrlButton(href, env)).rejects.toMatchObject({kind: "validation", message});
  expect(env.navigate).not.toHaveBeenCalled(); expect(env.openExternal).not.toHaveBeenCalled();
});

test("unsupported same-origin paths and route parameters retain router errors", async () => {
  const env = harness();
  await expect(runUrlButton("/other", env)).rejects.toMatchObject({kind: "validation", message: "Invalid navigation parameter: path."});
  await expect(runUrlButton("/erp?unknown=yes", env)).rejects.toMatchObject({kind: "validation", message: "Invalid navigation parameter: url."});
  expect(env.navigate).not.toHaveBeenCalled();
  expect(env.openExternal).not.toHaveBeenCalled();
  env.navigate.mockRejectedValue(new ClientError("validation", "Unsaved changes"));
  await expect(runUrlButton("/erp", env)).rejects.toMatchObject({ kind: "validation", message: "Unsaved changes" });
});

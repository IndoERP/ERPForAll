/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { MessageServices } from "../src/client/messages.js";
import { clientActions } from "../src/client/registry.js";
import type { RpcClient } from "../src/client/transport.js";
import "../src/client/identity-provider.js";

afterEach(() => { vi.unstubAllGlobals(); document.body.replaceChildren(); });

const ready = {
  configured: true, enabled: false, neutralized: false, label: "Company SSO",
  issuer: "https://identity.example.test", callback_url: "https://erp.example.test/erp/login/external/callback",
  keys: { mode: "remote", status: "ready", key_count: 2, last_success_unix: 1700000000, last_attempt_unix: 1700000000 },
};
async function open() {
  const host = document.createElement("main"), pending: Promise<void>[] = [];
  document.body.append(host);
  const handle = await clientActions.get("base.identity_provider").create({
    host, panel: host, controls: host, rpc: {} as RpcClient, companies: [],
    run: work => { pending.push(work()); }, notice: () => {}, messages: new MessageServices(host, error => { throw error; }),
    editorLocale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrfToken: "fixture",
    action: { id: 1, name: "Single Sign-On", tag: "base.identity_provider", params: {} },
    restored: null, open: async () => {},
  });
  return { host, handle, pending };
}

test("administrator can inspect cached keys, enable the provider and refresh its status", async () => {
  const fetcher = vi.fn()
    .mockResolvedValueOnce(Response.json(ready))
    .mockResolvedValueOnce(new Response(null, { status: 204 }))
    .mockResolvedValueOnce(Response.json({ ...ready, enabled: true }));
  vi.stubGlobal("fetch", fetcher);
  const { host, handle, pending } = await open();
  expect(host.textContent).toContain("HTTPS key set");
  expect(host.textContent).toContain(ready.callback_url);
  const toggle = Array.from(host.querySelectorAll("button")).find(b => b.textContent === "Enable provider");
  toggle!.click();
  await Promise.all(pending);
  const request = fetcher.mock.calls[1]![1] as RequestInit;
  expect(request.method).toBe("POST");
  expect(new URLSearchParams(String(request.body)).get("csrf_token")).toBe("fixture");
  expect(host.textContent).toContain("Disable provider");
  handle.dispose();
});

test.each([
  [{ ...ready, neutralized: true }, "Disabled on this neutralized database"],
  [{ configured: false, neutralized: false }, "Configure an OpenID Connect provider"],
])("reports an unavailable provider without activation controls", async (status, expected) => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(Response.json(status)));
  const { host, handle } = await open();
  expect(host.textContent).toContain(expected);
  expect(host.querySelectorAll("button")).toHaveLength(1);
  handle.dispose();
});

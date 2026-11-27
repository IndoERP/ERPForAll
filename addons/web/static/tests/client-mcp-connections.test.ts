/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { mcpConnections } from "../src/client/mcp-connections.js";
import type { RpcClient } from "../src/client/transport.js";
const item = { id: 33, name: "Reporting", read_only: true, models: ["sale.order"], methods: ["search_read"],
  expires_at: "2027-01-15T08:00:00Z", expired: false, last_used_at: null };
function fixture() {
  let items: (typeof item)[] = [];
  const call = vi.fn(async (_: string, method: string, args: Record<string, unknown>) => {
    if (method === "erp_mcp_connections") return { db: "tenant", login: "viewer", items, next: null };
    if (method === "erp_revoke_mcp_connection" && args["connection_id"] === 33) { items = []; return true; }
    throw new Error(method);
  });
  const view = mcpConnections({ rpc: { call } as unknown as RpcClient, context: {} });
  document.body.append(view.node);
  const click = (name: string) => {
    const found = [...view.node.querySelectorAll("button")].find(b => (b.getAttribute("aria-label") ?? b.textContent) === name && !b.hidden);
    if (!found) throw new Error(`Button not found: ${name}`);
    found.click();
  };
  const field = (name: string) => Array.from(view.node.querySelectorAll("label")).find(l => l.textContent?.startsWith(name))!.querySelector("input")!;
  return { ...view, click, field, call, add: () => { items = [item]; } };
}
afterEach(() => { document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

test("the owner creates a read-only scoped connection, copies the token once, and explicitly revokes by id", async () => {
  const v = fixture();
  const token = crypto.randomUUID();
  const writeText = vi.fn(async () => undefined);
  vi.stubGlobal("navigator", { clipboard: { writeText } });
  const fetcher = vi.fn(async (_url: string, options: RequestInit) => {
    const args = JSON.parse(String(options.body));
    expect(args.mcp_access).toEqual({ read_only: true, models: ["sale.order"], methods: ["search_read"] });
    expect([args.db, args.login, args.device_name, args.audience]).toEqual(["tenant", "viewer", "Reporting", "mcp"]);
    expect(options.cache).toBe("no-store"); expect(options.credentials).toBe("omit");
    v.add();
    return Response.json({ api_key: token, expires_at: item.expires_at });
  });
  vi.stubGlobal("fetch", fetcher);
  v.click("Manage MCP connections"); await vi.waitFor(() => expect(v.node.textContent).toContain("No MCP connections."));
  v.click("New MCP connection"); v.field("Connection name").value = "Reporting";
  v.field("Models").value = "sale.order"; v.field("Methods").value = "search_read";
  v.field("Confirm your ERP password").value = "fixture-only";
  v.click("Create connection");
  await vi.waitFor(() => expect(v.field("One-time token").value === token).toBe(true));
  expect(v.field("Confirm your ERP password").value).toBe("");
  expect(v.node.querySelector("pre")!.textContent).toContain("/erp/mcp");
  expect(v.node.textContent!.includes(token)).toBe(false);
  v.click("Copy token"); expect(writeText.mock.calls.length).toBe(1);
  const secretField = v.field("One-time token");
  v.click("Done"); expect(secretField.value).toBe("");
  expect(v.node.querySelector(".mcp-list")!.textContent).toContain("Models: sale.order");
  v.click("Revoke Reporting");
  expect(v.call.mock.calls.some(([,m]) => m === "erp_revoke_mcp_connection")).toBe(false);
  v.click("Confirm revocation"); await vi.waitFor(() => expect(v.node.textContent).toContain("Revoked Reporting."));
  expect(v.call.mock.calls.some(([,method, args]) => method === "erp_revoke_mcp_connection" && args["connection_id"] === 33)).toBe(true);
  expect(v.node.textContent!.includes(token)).toBe(false); v.dispose();
});

test("disposing the preferences page during issuance clears the password and suppresses delayed secrets", async () => {
  const v = fixture();
  let resolve!: (r: Response) => void; const pending = new Promise<Response>(r => { resolve = r; });
  vi.stubGlobal("fetch", () => pending);
  v.click("Manage MCP connections"); await vi.waitFor(() => expect(v.node.textContent).toContain("No MCP connections."));
  v.click("New MCP connection"); v.field("Connection name").value = "Test"; v.field("Confirm your ERP password").value = "test-password";
  v.click("Create connection"); v.dispose();
  resolve(Response.json({ api_key: crypto.randomUUID(), expires_at: item.expires_at }));
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(document.body.textContent).toBe(""); expect(v.node.querySelector(".mcp-receipt")!.textContent).toBe("");
  expect(v.field("Confirm your ERP password").value).toBe("");
});

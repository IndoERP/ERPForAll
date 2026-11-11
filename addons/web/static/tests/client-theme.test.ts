/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { loadDatabaseTheme } from "../src/client/theme.js";
import { RpcClient } from "../src/client/transport.js";

const key = "theme_office_blue.appearance";
function client(installed: boolean, value: unknown = "office-blue", registry = "r1") {
  const fetch = vi.fn(async (path: unknown, _init?: RequestInit) => new Response(JSON.stringify(String(path).endsWith("erp_describe")
    ? { wire_major: 1, registry_version: registry, scope: { kind: "global" }, declarations: installed ? [{ key, kind: "selection" }] : [] }
    : { wire_major: 1, scope: { kind: "global" }, revision: "1", values: [{ key, value, source: "stored" }] }), { headers: { "Content-Type": "application/json" } }));
  return { fetch, rpc: new RpcClient(fetch, () => {}) };
}
afterEach(() => { document.documentElement.removeAttribute("data-office-theme"); document.getElementById("database-appearance-stylesheet")?.remove(); });

test("installed database preference selects Office Blue without browser persistence", async () => {
  const { rpc, fetch } = client(true);
  await loadDatabaseTheme(rpc, "r1", {}, async () => {});
  expect(document.documentElement.dataset["officeTheme"]).toBe("office-blue");
  expect(JSON.parse(String(fetch.mock.calls[1]?.[1]?.body))).toMatchObject({ scope: { kind: "global" }, keys: [key] });
});

test("another database with no installed theme clears the previous appearance without reading orphan values", async () => {
  await loadDatabaseTheme(client(true).rpc, "r1", {}, async () => {});
  const { rpc, fetch } = client(false);
  await loadDatabaseTheme(rpc, "r1", {}, async () => {});
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
  expect(fetch).toHaveBeenCalledTimes(1);
});

test("default selection clears an existing theme", async () => {
  document.documentElement.dataset["officeTheme"] = "office-blue";
  await loadDatabaseTheme(client(true, "default").rpc, "r1", {}, async () => {});
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});

test.each(["unknown", false, null])("invalid theme %j is reported instead of silently selecting default", async value => {
  await expect(loadDatabaseTheme(client(true, value).rpc, "r1", {}, async () => {})).rejects.toMatchObject({ kind: "server", message: "Invalid database appearance setting." });
});

test("stale declaration registry requires reload", async () => {
  await expect(loadDatabaseTheme(client(true, "office-blue", "r2").rpc, "r1", {}, async () => {})).rejects.toMatchObject({ kind: "unsupported", message: "Settings changed. Reload the application." });
});

test("RPC failures propagate to the shell error surface", async () => {
  const rpc = new RpcClient(async () => { throw new Error("offline"); }, () => {});
  await expect(loadDatabaseTheme(rpc, "r1", {}, async () => {})).rejects.toBeDefined();
});

test("asset failure prevents applying a selected theme", async () => {
  await expect(loadDatabaseTheme(client(true).rpc, "r1", {}, async () => { throw new Error("missing CSS"); })).rejects.toThrow("missing CSS");
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});


test("the tenant stylesheet must finish loading before its palette is applied", async () => {
  const loading = loadDatabaseTheme(client(true).rpc, "r1", {});
  await vi.waitFor(() => expect(document.querySelector("#database-appearance-stylesheet")).not.toBeNull());
  const link = document.querySelector<HTMLLinkElement>("#database-appearance-stylesheet")!;
  expect(link.getAttribute("href")).toBe("/theme_office_blue/static/src/theme.css");
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
  link.dispatchEvent(new Event("load"));
  await loading;
  expect(document.documentElement.dataset["officeTheme"]).toBe("office-blue");
  await loadDatabaseTheme(client(false).rpc, "r1", {});
  expect(document.querySelector("#database-appearance-stylesheet")).toBeNull();
});

test("a missing tenant stylesheet reports a precise error and removes its link", async () => {
  const loading = loadDatabaseTheme(client(true).rpc, "r1", {});
  const rejection = expect(loading).rejects.toMatchObject({ kind: "server", message: "Office Blue stylesheet could not be loaded. Reload the application." });
  await vi.waitFor(() => expect(document.querySelector("#database-appearance-stylesheet")).not.toBeNull());
  document.querySelector("#database-appearance-stylesheet")!.dispatchEvent(new Event("error"));
  await rejection;
  expect(document.querySelector("#database-appearance-stylesheet")).toBeNull();
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});

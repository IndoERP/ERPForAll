/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { loadDatabaseTheme, officeShell } from "../src/client/theme.js";
import { RpcClient } from "../src/client/transport.js";

const key = "theme_office_blue.appearance";
const officeChoices = [["default", "Default"], ["office-blue", "Office Blue"]];
type Declaration = { key: string; kind: string; choices: unknown };
const appearance = (owner: string, value: string, label: string): Declaration => ({ key: `${owner}.appearance`, kind: "selection", choices: [["default", "Default"], [value, label]] });
function server(declarations: Declaration[], values: Record<string, unknown>, registry = "r1") {
  const fetch = vi.fn(async (path: unknown, _init?: RequestInit) => new Response(JSON.stringify(String(path).endsWith("erp_describe")
    ? { wire_major: 1, registry_version: registry, scope: { kind: "global" }, declarations }
    : { wire_major: 1, scope: { kind: "global" }, revision: "1", values: Object.entries(values).map(([k, value]) => ({ key: k, value, source: "stored" })) }), { headers: { "Content-Type": "application/json" } }));
  return { fetch, rpc: new RpcClient(fetch, () => {}) };
}
function client(installed: boolean, value: unknown = "office-blue", registry = "r1") {
  return server(installed ? [{ key, kind: "selection", choices: officeChoices }] : [], { [key]: value }, registry);
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
  const rejection = expect(loading).rejects.toMatchObject({ kind: "server", message: "The database appearance stylesheet could not be loaded. Reload the application." });
  await vi.waitFor(() => expect(document.querySelector("#database-appearance-stylesheet")).not.toBeNull());
  document.querySelector("#database-appearance-stylesheet")!.dispatchEvent(new Event("error"));
  await rejection;
  expect(document.querySelector("#database-appearance-stylesheet")).toBeNull();
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});

test("each installed appearance addon contributes its own stylesheet and the selected one is applied", async () => {
  const { rpc, fetch } = server([appearance("theme_office_blue", "office-blue", "Office Blue"), appearance("theme_warm", "warm", "Warm"), { key: "sale.mode", kind: "selection", choices: [["a", "A"]] }],
    { "theme_office_blue.appearance": "default", "theme_warm.appearance": "warm" });
  const loaded: string[] = [];
  await loadDatabaseTheme(rpc, "r1", {}, async href => { loaded.push(href); });
  expect(loaded).toEqual(["/theme_warm/static/src/theme.css"]);
  expect(document.documentElement.dataset["officeTheme"]).toBe("warm");
  expect(fetch).toHaveBeenCalledTimes(2);
  expect(JSON.parse(String(fetch.mock.calls[1]?.[1]?.body)).keys).toEqual(["theme_office_blue.appearance", "theme_warm.appearance"]);
});

test("two selected appearances are refused instead of picking one", async () => {
  const { rpc } = server([appearance("theme_office_blue", "office-blue", "Office Blue"), appearance("theme_warm", "warm", "Warm")],
    { "theme_office_blue.appearance": "office-blue", "theme_warm.appearance": "warm" });
  const loaded: string[] = [];
  await expect(loadDatabaseTheme(rpc, "r1", {}, async href => { loaded.push(href); })).rejects.toMatchObject({ kind: "server", message: "More than one database appearance is selected. Choose one in Settings." });
  expect(loaded).toEqual([]);
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});

test("settings that are not appearances are never read", async () => {
  const { rpc, fetch } = server([{ key: "sale.mode", kind: "selection", choices: [["a", "A"]] }], {});
  await loadDatabaseTheme(rpc, "r1", {}, async () => {});
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(document.documentElement.hasAttribute("data-office-theme")).toBe(false);
});

test.each([
  ["no default choice", { key: "theme_warm.appearance", kind: "selection", choices: [["warm", "Warm"]] }],
  ["a choice that is not an attribute token", { key: "theme_warm.appearance", kind: "selection", choices: [["default", "Default"], ["Warm Theme", "Warm"]] }],
  ["a non-selection kind", { key: "theme_warm.appearance", kind: "string", choices: null }],
  ["missing choices", { key: "theme_warm.appearance", kind: "selection", choices: null }],
])("an appearance declaration with %s is reported", async (_name, declaration) => {
  const { rpc } = server([declaration], { "theme_warm.appearance": "default" });
  await expect(loadDatabaseTheme(rpc, "r1", {}, async () => {})).rejects.toMatchObject({ kind: "server", message: "Invalid database appearance declaration." });
});

test("a value outside the declared choices is reported", async () => {
  const { rpc } = server([appearance("theme_warm", "warm", "Warm")], { "theme_warm.appearance": "office-blue" });
  await expect(loadDatabaseTheme(rpc, "r1", {}, async () => {})).rejects.toMatchObject({ kind: "server", message: "Invalid database appearance setting." });
});

test("the office shell is on exactly while an appearance is applied", async () => {
  expect(officeShell()).toBe(false);
  await loadDatabaseTheme(server([appearance("theme_warm", "warm", "Warm")], { "theme_warm.appearance": "warm" }).rpc, "r1", {}, async () => {});
  expect(officeShell()).toBe(true);
  await loadDatabaseTheme(client(false).rpc, "r1", {}, async () => {});
  expect(officeShell()).toBe(false);
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { MetadataCache, invalidatesMetadata } from "../src/client/metadata-cache.js";

test("identical reads share one request, including one still in flight", async () => {
  const cache = new MetadataCache();
  let release!: (value: unknown) => void;
  const load = vi.fn(() => new Promise(resolve => { release = resolve; }));
  const first = cache.read("k", load), second = cache.read("k", load);
  release({ name: "Notes" });
  expect(await first).toEqual({ name: "Notes" });
  expect(await second).toEqual({ name: "Notes" });
  expect(await cache.read("k", load)).toEqual({ name: "Notes" });
  expect(load).toHaveBeenCalledOnce();
});

test("every reader gets its own copy, so a caller cannot change the shared answer", async () => {
  const cache = new MetadataCache();
  const load = async () => ({ views: [{ type: "list" }] });
  const first = await cache.read("k", load) as { views: { type: string }[] };
  first.views[0]!.type = "changed";
  expect(await cache.read("k", load)).toEqual({ views: [{ type: "list" }] });
});

test("a failed request reaches its callers and is never kept", async () => {
  const cache = new MetadataCache();
  const failing = vi.fn(async () => { throw new Error("denied"); });
  await expect(cache.read("k", failing)).rejects.toThrow("denied");
  expect(cache.size).toBe(0);
  expect(await cache.read("k", async () => "fresh")).toBe("fresh");
  expect(failing).toHaveBeenCalledOnce();
});

test("the cache stays within its bound, evicting the least recently used entry", async () => {
  const cache = new MetadataCache(2);
  const load = vi.fn(async () => 1);
  await cache.read("a", load); await cache.read("b", load);
  await cache.read("a", load); // a becomes most recent
  await cache.read("c", load); // evicts b
  expect(cache.size).toBe(2);
  expect(load).toHaveBeenCalledTimes(3);
  await cache.read("a", load);
  expect(load).toHaveBeenCalledTimes(3);
  await cache.read("b", load);
  expect(load).toHaveBeenCalledTimes(4);
  expect(() => new MetadataCache(0)).toThrow(RangeError);
  expect(() => new MetadataCache(1.5)).toThrow(RangeError);
});

test("clear drops every entry, and a request that finishes afterwards is not stored", async () => {
  const cache = new MetadataCache();
  let release!: (value: unknown) => void;
  const pending = cache.read("k", () => new Promise(resolve => { release = resolve; }));
  cache.clear();
  release("old");
  expect(await pending).toBe("old");
  expect(cache.size).toBe(0);
  expect(await cache.read("k", async () => "new")).toBe("new");
});

test("technical, user and group writes invalidate metadata; business writes do not", () => {
  for (const model of ["ir.ui.view", "ir.actions.act_window", "ir.ui.menu", "ir.config.setting", "ir.module.module", "res.users", "res.groups"]) {
    expect(invalidatesMetadata(model)).toBe(true);
  }
  for (const model of ["res.partner", "res.company", "acceptance_notes.note", "irregular.model"]) {
    expect(invalidatesMetadata(model)).toBe(false);
  }
});


test("bundle seeding is generation-bound and preserves an existing producer", async () => {
  const cache = new MetadataCache(2);
  const generation = cache.generation;
  let release!: (value: unknown) => void;
  const pending = cache.read("views", () => new Promise(resolve => { release = resolve; }));
  cache.seed("views", { name: "bundle" }, generation);
  release({ name: "existing" });
  expect(await pending).toEqual({ name: "existing" });
  expect(await cache.read("views", async () => "wrong")).toEqual({ name: "existing" });
  cache.clear();
  cache.seed("stale", "old", generation);
  expect(cache.size).toBe(0);
  cache.seed("fresh", { name: "fresh" }, cache.generation);
  const value = await cache.read("fresh", async () => "wrong") as { name: string };
  value.name = "mutated";
  expect(await cache.read("fresh", async () => "wrong")).toEqual({ name: "fresh" });
  cache.seed("second", 2, cache.generation);
  cache.seed("third", 3, cache.generation);
  expect(cache.size).toBe(2);
});

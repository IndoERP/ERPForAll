/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Local records: same store, never sent (spec messaging-client-store.md).
import { effect, flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Harness, failsWith, frameText } from "./support";

test("local records live in the same keyed tables with string ids", () => {
  const h = new Harness();
  const drafts = h.store.defineLocalModel("ui.draft", ["text", "thread"]);
  const seen: unknown[] = [];
  const stop = effect(() => {
    seen.push(drafts.get("a", "text"));
  });
  flushSync();
  drafts.create("a", { text: "hello" });
  flushSync();
  drafts.set("a", { text: "hello world", thread: 7 });
  flushSync();
  expect(seen).toEqual([undefined, "hello", "hello world"]);
  expect(h.store.record("ui.draft", "a").get("thread")).toBe(7);
  h.store.record("ui.draft", "a").set("text", "through the handle");
  expect(drafts.get("a", "text")).toBe("through the handle");
  drafts.set("a", { text: "hello world" });
  expect(drafts.has("a")).toBe(true);
  expect(drafts.ids()).toEqual(["a"]);
  expect(h.store.record("ui.draft", "a").exists).toBe(true);
  drafts.delete("a");
  flushSync();
  expect(seen).toEqual([undefined, "hello", "hello world", undefined]);
  expect(drafts.ids()).toEqual([]);
  drafts.create("a");
  expect(drafts.get("a", "text")).toBeUndefined();
  stop();

  failsWith(() => drafts.create("a"), "bad_local", 'ui.draft.create: local record "a" already exists');
  failsWith(() => drafts.set("zz", { text: "x" }), "bad_local", 'ui.draft.set: no local record "zz"');
  failsWith(() => drafts.delete("zz"), "bad_local", 'ui.draft.delete: no local record "zz"');
  failsWith(() => drafts.create("b", { nope: 1 }), "unknown_field", "ui.draft.create: nope is not a field of local model ui.draft");
  failsWith(() => drafts.get("a", "nope"), "unknown_field", "ui.draft.get: nope is not a field of local model ui.draft");
  failsWith(() => drafts.create(3 as unknown as string), "bad_id", "ui.draft.create: a local record id must be a non-empty string, got 3");
  failsWith(() => h.store.defineLocalModel("res.partner", ["x"]), "local_model_clash", "res.partner is a schema model; a local model needs its own name");
  failsWith(() => h.store.defineLocalModel("ui.draft", ["x"]), "bad_local", "local model ui.draft is already defined");
  failsWith(() => h.store.defineLocalModel("", ["x"]), "bad_local", 'local model name must be a non-empty string without "$", got ""');
  failsWith(() => h.store.defineLocalModel("ui.empty", []), "bad_local", "fields of local model ui.empty must be a non-empty list of unique non-empty strings");
  failsWith(() => h.store.local("ui.none"), "unknown_model", "ui.none is not a local model of this store");
});

test("local records never appear in resync requests, payload results or exports of server data", () => {
  const h = new Harness();
  const drafts = h.store.defineLocalModel("ui.draft", ["text", "thread"]);
  drafts.create("1", { text: "secret draft" });

  h.answer(10, { "mail.thread": [{ id: 1, capped: 5 }], "res.partner": [{ id: 1, name: "Ada" }] });
  const stale = h.frame(20, 1, { "mail.thread": [{ id: 1, capped: { inc: -1 } }] });
  const refusedUnknown = h.store.applyFrame(frameText(30, 2, { "ui.draft": [{ id: 1, text: "forged" }] }));
  const refusedField = h.store.applyFrame(frameText(31, 3, { "res.partner": [{ id: 1, text: "wrong" }] }));
  // The store builds resync requests from server data only. (The refusal of a payload that names a local model
  // quotes the payload's own text, so it is left out of the check.)
  const built = JSON.stringify([stale, refusedField]);
  expect(built).not.toContain("ui.draft");
  expect(built).not.toContain("secret draft");
  expect(stale).toMatchObject({ status: "applied", resync: [{ reason: "counter_stale", model: "mail.thread" }] });
  expect(refusedUnknown).toMatchObject({ status: "refused", resync: [{ reason: "unknown_model" }] });
  // A payload that names a local model changed nothing: the record keeps its value.
  expect(drafts.get("1", "text")).toBe("secret draft");

  expect(h.store.persistable("ui.draft")).toEqual([{ id: "1", values: { text: "secret draft", thread: undefined } }]);
  failsWith(() => h.store.persistable("res.partner"), "not_local", "res.partner is a server model; only local models can be persisted by the client");
  failsWith(() => h.store.persistable("nope"), "unknown_model", "nope is not a local model of this store");

  const clash = JSON.stringify({ wire: 1, schema: "g3", models: { "ui.draft": { fields: { text: { kind: "string" } } } } });
  failsWith(() => h.store.setSchema(clash), "local_model_clash", "schema g3 declares ui.draft, which is a local model of this store");
  expect(h.store.generation).toBe("g1");
  expect(drafts.get("1", "text")).toBe("secret draft");
});

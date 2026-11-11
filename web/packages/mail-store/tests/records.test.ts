/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Identity, typed reads, projection from the wire applier, counters and deletes (spec messaging-client-store.md).
import { effect, flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Harness, failsWith } from "./support";
import type { ModelName, RecordHandle } from "../src/index";

test("equal ids of two models never collide", () => {
  const h = new Harness();
  h.answer(10, { "mail.message": [{ id: 1, body: "hello" }], "res.partner": [{ id: 1, name: "Ada" }] });
  h.frame(20, 1, { "mail.message": [{ id: 1, body: "edited" }] });
  const drafts = h.store.defineLocalModel("ui.draft", ["text", "thread"]);
  drafts.create("1", { text: "local draft" });

  expect(h.store.record("mail.message", 1).get("body")).toBe("edited");
  expect(h.store.record("res.partner", 1).get("name")).toBe("Ada");
  expect(h.store.record("ui.draft", "1").get("text")).toBe("local draft");
  expect(drafts.get("1", "text")).toBe("local draft");
  expect(h.store.record("res.partner", 1).exists).toBe(true);
  expect(h.store.record("res.partner", 2).exists).toBe(false);
  expect(h.store.record("res.partner", 2).get("name")).toBeUndefined();
});

test("typed accessors fail loudly on unknown models, ids, fields and field kinds", () => {
  const h = new Harness();
  h.answer(10, {
    "mail.message": [{ id: 5, body: "hi", author: 1, attachments: [3, 4] }],
    "res.partner": [{ id: 1, name: "Ada" }],
    "ir.attachment": [{ id: 3, name: "a.txt" }, { id: 4, name: "b.txt" }],
  });
  h.store.defineLocalModel("ui.draft", ["text", "thread"]);

  failsWith(() => h.store.record("no.such" as ModelName, 1), "unknown_model", "record: no.such is neither a model of schema g1 nor a local model");
  failsWith(() => h.store.record("res.partner", 1.5), "bad_id", "record(res.partner): a server record id must be a safe integer, got 1.5");
  failsWith(() => h.store.record("res.partner", "1"), "bad_id", 'record(res.partner): a server record id must be a safe integer, got 1');
  failsWith(() => h.store.record("ui.draft", 3), "bad_id", "record(ui.draft): a local record id must be a non-empty string, got 3");
  const partner = h.store.record("res.partner", 1);
  // @ts-expect-error nope is not a declared field of res.partner
  failsWith(() => partner.get("nope"), "unknown_field", "res.partner.nope is not a field of res.partner");
  failsWith(() => (partner as RecordHandle).one("name"), "field_kind", "res.partner.name is a string field, not a many2one field");
  const message = h.store.record("mail.message", 5);
  failsWith(() => (message as RecordHandle).many("author"), "field_kind", "mail.message.author is a many2one field, not an x2many field");
  failsWith(() => (message as RecordHandle).counter("body"), "field_kind", "mail.message.body is a string field, not a counter field");

  // Declared fields read with their declared types.
  const body: string | undefined = message.get("body");
  const authorId: number | null | undefined = message.get("author");
  const attachmentIds: readonly number[] | undefined = message.get("attachments");
  const author: RecordHandle<"res.partner"> | undefined = message.one("author");
  const attachments: RecordHandle<"ir.attachment">[] = message.many("attachments");
  expect(body).toBe("hi");
  expect(authorId).toBe(1);
  expect(attachmentIds).toEqual([3, 4]);
  expect(author?.get("name")).toBe("Ada");
  expect(attachments.map((a) => a.get("name"))).toEqual(["a.txt", "b.txt"]);
  expect(author?.is(h.store.record("res.partner", 1))).toBe(true);
  // A relation field holding nothing reads as empty, not as an error.
  expect(h.store.record("mail.message", 6).one("author")).toBeUndefined();
  expect(h.store.record("mail.message", 6).many("attachments")).toEqual([]);
});

test("reads show only what the applier holds: stale answers and deleted records stay out", () => {
  const h = new Harness();
  const message = h.store.record("mail.message", 5);
  h.frame(20, 1, { "mail.message": [{ id: 5, body: "from the frame" }] });
  // An answer read before transaction 20 completed (xmax 15) does not reflect the frame: its body is older.
  h.answer(15, { "mail.message": [{ id: 5, body: "older answer" }] });
  expect(message.get("body")).toBe("from the frame");
  // An answer that knows transaction 20 replaces it.
  h.answer(30, { "mail.message": [{ id: 5, body: "newer answer" }] });
  expect(message.get("body")).toBe("newer answer");

  // A request is outstanding when the delete arrives, so the applier keeps a tombstone.
  const request = h.store.beginRequest();
  h.frame(40, 2, { "mail.message": [{ id: 5, delete: true }] });
  expect(message.exists).toBe(false);
  expect(message.get("body")).toBeUndefined();
  h.answer(35, { "mail.message": [{ id: 5, body: "stale answer" }] });
  expect(message.exists).toBe(false);
  expect(message.get("body")).toBeUndefined();
  h.store.settleRequest(request);
});

test("re-applying a payload changes nothing and does not rerun readers", () => {
  const h = new Harness();
  const message = h.store.record("mail.message", 5);
  const seen: unknown[] = [];
  const stop = effect(() => {
    seen.push(message.get("body"));
  });
  flushSync();
  expect(seen).toEqual([undefined]);

  const frame = { "mail.message": [{ id: 5, body: "one", author: 1 }] };
  h.frame(20, 1, frame);
  flushSync();
  const held = JSON.stringify(h.store.record("mail.message", 5).get("author"));
  h.frame(20, 1, frame);
  flushSync();
  expect(seen).toEqual([undefined, "one"]);
  expect(JSON.stringify(h.store.record("mail.message", 5).get("author"))).toBe(held);

  // A change of another field of the record leaves the reader alone; a change of its own field reruns it.
  h.frame(21, 2, { "mail.message": [{ id: 5, author: 2 }] });
  flushSync();
  expect(seen).toEqual([undefined, "one"]);
  h.frame(22, 3, { "mail.message": [{ id: 5, body: "two" }] });
  flushSync();
  expect(seen).toEqual([undefined, "one", "two"]);
  stop();
});

test("counters show what the applier answers and a stale one asks for a resync", () => {
  const h = new Harness();
  const thread = h.store.record("mail.thread", 1);
  expect(thread.counter("capped")).toBeUndefined();
  h.answer(10, { "mail.thread": [{ id: 1, capped: 5, unread: 2 }] });
  expect(thread.counter("capped")).toEqual({ value: 5, saturated: true, stale: false });
  expect(thread.counter("unread")).toEqual({ value: 2, saturated: false, stale: false });

  const result = h.frame(20, 1, { "mail.thread": [{ id: 1, capped: { inc: -1 }, unread: { inc: 1 } }] });
  expect(result).toEqual({
    status: "applied",
    resync: [{ reason: "counter_stale", model: "mail.thread", id: 1, field: "capped", counterReason: "saturated_decrement" }],
  });
  expect(thread.counter("capped")).toEqual({ value: null, saturated: false, stale: true });
  expect(thread.counter("unread")).toEqual({ value: 3, saturated: false, stale: false });

  // An answer from a request sent after the dropped delta clears it.
  const request = h.store.beginRequest();
  h.answer(30, { "mail.thread": [{ id: 1, capped: 4 }] }, request);
  expect(thread.counter("capped")).toEqual({ value: 4, saturated: false, stale: false });
  h.store.settleRequest(request);
});

test("a delete removes the record from its table, from windows and from lists that held it", () => {
  const h = new Harness();
  const thread = { model: "mail.thread", id: 1 };
  h.answer(10, {
    "mail.message": [{ id: 5, body: "a" }, { id: 6, body: "b" }],
    "res.partner": [{ id: 5, name: "same id, other model" }],
    "mail.thread": [{ id: 1, messages: [5, 6] }],
  });
  h.store.windows.replace(thread, [5, 6], false);
  const lists: unknown[] = [];
  const stop = effect(() => {
    lists.push(h.store.record("mail.thread", 1).get("messages"));
  });
  flushSync();
  expect(lists).toEqual([[5, 6]]);

  h.frame(20, 1, { "mail.message": [{ id: 5, delete: true }] });
  flushSync();
  expect(h.store.record("mail.message", 5).exists).toBe(false);
  expect(h.store.record("mail.message", 5).get("body")).toBeUndefined();
  expect(h.store.record("mail.message", 6).get("body")).toBe("b");
  expect(h.store.windows.get(thread)?.ids).toEqual([6]);
  expect(h.store.record("mail.thread", 1).get("messages")).toEqual([6]);
  expect(lists).toEqual([[5, 6], [6]]);
  expect(h.store.record("res.partner", 5).get("name")).toBe("same id, other model");
  stop();
});

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Eviction, collection, refetch and the D-6 retention rule (spec messaging-client-store.md).
import { MessagingStore } from "../src/index";
import { expect, test } from "vitest";
import { Harness, answerText, failsWith, frameText } from "./support";

const thread = (id: number): { model: string; id: number } => ({ model: "mail.thread", id });
const range = (from: number, to: number): number[] => Array.from({ length: to - from + 1 }, (_, i) => from + i);
const messages = (ids: readonly number[], body = "m"): { id: number; body: string }[] => ids.map((id) => ({ id, body: `${body}${id}` }));

test("eviction drops the least recently used windows and keeps what a displayed surface holds", () => {
  const h = new Harness({ maxThreads: 2 });
  const w = h.store.windows;
  const ids = (n: number): number[] => [n * 10, n * 10 + 1];
  h.answer(10, { "mail.message": [1, 2, 3, 4, 5].flatMap((n) => messages(ids(n))) });
  for (const n of [1, 2, 3, 4, 5]) w.replace(thread(n), ids(n), false);

  const lease = h.store.hold("mail.thread", 1);
  for (const n of [2, 3, 4, 5, 3]) w.touch(thread(n)); // thread 1 is now the least recently used, but held
  expect(h.store.collect()).toEqual({ windows: 2, evicted: { "mail.message": 4 } });
  expect(w.threads().map((t) => t.id).sort()).toEqual([1, 3, 5]);
  for (const n of [1, 3, 5]) for (const id of ids(n)) expect(h.store.record("mail.message", id).exists).toBe(true);
  for (const n of [2, 4]) for (const id of ids(n)) expect(h.store.record("mail.message", id).exists).toBe(false);
  expect(w.get(thread(2))).toBeUndefined();

  // Once the surface lets go, the thread is an ordinary candidate at the next pass.
  lease.release();
  for (const n of [5, 3]) w.touch(thread(n));
  expect(h.store.collect()).toEqual({ windows: 1, evicted: { "mail.message": 2 } });
  expect(w.threads().map((t) => t.id).sort()).toEqual([3, 5]);
  expect(h.store.record("mail.message", 10).exists).toBe(false);
  expect(h.store.record("mail.message", 30).exists).toBe(true);
});

test("partners and attachments that nothing retained references are collected", () => {
  const h = new Harness();
  h.answer(10, {
    "res.partner": [1, 3, 4, 5, 6].map((id) => ({ id, name: `p${id}` })),
    "ir.attachment": [1, 2].map((id) => ({ id, name: `a${id}` })),
    "mail.message": [
      { id: 100, body: "kept", author: 1, attachments: [1] },
      { id: 200, body: "in no window", author: 6 },
    ],
    "discuss.member": [{ id: 1, partner: 4 }],
  });
  h.store.windows.replace(thread(1), [100], false);
  h.store.hold("res.partner", 5);

  expect(h.store.collect().evicted).toEqual({ "mail.message": 1, "res.partner": 2, "ir.attachment": 1 });
  const exists = (model: "res.partner" | "ir.attachment" | "mail.message", id: number): boolean => h.store.record(model, id).exists;
  expect([1, 3, 4, 5, 6].map((id) => exists("res.partner", id))).toEqual([true, false, true, true, false]);
  expect([1, 2].map((id) => exists("ir.attachment", id))).toEqual([true, false]);
  expect([100, 200].map((id) => exists("mail.message", id))).toEqual([true, false]);
  // A model that is neither a message nor collectable is a root: its record stays and keeps its partner.
  expect(h.store.record("discuss.member", 1).exists).toBe(true);
  expect(h.store.record("mail.message", 100).one("author")?.get("name")).toBe("p1");
});

test("a thread refetched after eviction is contiguous", () => {
  const h = new Harness({ maxThreads: 1 });
  const w = h.store.windows;
  h.answer(10, { "mail.message": messages(range(11, 40)) });
  w.replace(thread(1), range(11, 40), true);
  w.replace(thread(2), [], false);
  expect(h.store.collect().windows).toBe(1);
  expect(w.get(thread(1))).toBeUndefined();
  expect(h.store.record("mail.message", 11).exists).toBe(false);

  // Newer messages arrive by frames while the thread has no window: nothing is held for them.
  h.frame(20, 1, { "mail.message": messages(range(41, 50), "live") });
  for (const id of range(41, 50)) expect(w.appendLive(thread(1), id)).toBe("no_window");

  // The thread view fetches the latest page and then pages older.
  h.answer(30, { "mail.message": messages(range(21, 50)) });
  w.replace(thread(1), range(21, 50), true);
  expect(w.get(thread(1))).toEqual({ ids: range(21, 50), hasOlder: true, hasNewer: false });
  h.answer(31, { "mail.message": messages(range(11, 20)) });
  w.prependOlder(thread(1), range(11, 20), false);
  expect(w.get(thread(1))).toEqual({ ids: range(11, 50), hasOlder: false, hasNewer: false });
  for (const id of range(11, 50)) expect(h.store.record("mail.message", id).exists).toBe(true);
});

test("an evicted record keeps its revisions until the requests outstanding at eviction settle", () => {
  const h = new Harness();
  h.frame(20, 1, { "mail.message": [{ id: 5, body: "from the frame" }] });
  h.answer(10, { "mail.message": [{ id: 7, body: "quiet" }] });
  const request = h.store.beginRequest();
  expect(h.store.collect().evicted).toEqual({ "mail.message": 2 });
  const five = h.store.record("mail.message", 5);
  const seven = h.store.record("mail.message", 7);
  expect([five.exists, seven.exists]).toEqual([false, false]);

  // An answer that does not reflect the frame cannot bring older data back while the request is outstanding.
  h.answer(15, { "mail.message": [{ id: 5, body: "stale" }] });
  expect(five.exists).toBe(false);
  expect(five.get("body")).toBeUndefined();
  // An answer with the same revision as the retained value confirms it: a refetch of a quiet server shows it.
  h.answer(10, { "mail.message": [{ id: 7, body: "quiet" }] });
  expect(seven.get("body")).toBe("quiet");

  // After the settle the retired record is released: an older answer applies as for a record never seen.
  h.store.settleRequest(request);
  h.answer(15, { "mail.message": [{ id: 5, body: "after release" }] });
  expect(five.get("body")).toBe("after release");

  // With no request outstanding an evicted record is dropped at once.
  h.frame(30, 2, { "mail.message": [{ id: 6, body: "six" }] });
  expect(h.store.collect().evicted).toEqual({ "mail.message": 3 });
  h.answer(15, { "mail.message": [{ id: 6, body: "again" }] });
  expect(h.store.record("mail.message", 6).get("body")).toBe("again");

  // Revival by a frame that mentions one field only: the store projects every field the applier still holds.
  const lease = h.store.hold("res.partner", 1);
  h.answer(40, { "res.partner": [{ id: 1, name: "Ada" }], "mail.message": [{ id: 8, body: "eight", author: 1 }] });
  h.store.windows.replace(thread(1), [8], false);
  const pending = h.store.beginRequest();
  h.store.windows.drop(thread(1));
  h.store.collect();
  const eight = h.store.record("mail.message", 8);
  expect(eight.exists).toBe(false);
  h.frame(50, 3, { "mail.message": [{ id: 8, body: "eight, edited" }] });
  expect(eight.exists).toBe(true);
  expect(eight.get("body")).toBe("eight, edited");
  expect(eight.get("author")).toBe(1);
  expect(eight.one("author")?.get("name")).toBe("Ada");
  lease.release();
  h.store.windows.replace(thread(1), [8], false);
  h.store.collect();
  expect(h.store.record("res.partner", 1).exists).toBe(true);
  h.store.settleRequest(pending);
});

test("a deleted record stays deleted until the requests outstanding at the delete settle, and settling collects", () => {
  const h = new Harness({ collectOnSettle: true });
  h.answer(10, { "mail.message": messages([5, 6]) });
  h.store.windows.replace(thread(1), [6], false);
  const request = h.store.beginRequest();
  h.frame(20, 1, { "mail.message": [{ id: 5, delete: true }] });
  h.answer(15, { "mail.message": [{ id: 5, body: "stale" }] });
  expect(h.store.record("mail.message", 5).exists).toBe(false);

  h.store.settleRequest(request);
  expect(h.store.record("mail.message", 6).exists).toBe(true);
  // The tombstone is released with the settle: an older answer is now taken as a record never seen.
  h.answer(15, { "mail.message": [{ id: 5, body: "after release" }] });
  expect(h.store.record("mail.message", 5).get("body")).toBe("after release");

  // Settling collects: a message no window holds goes with the settle, unless collectOnSettle is off.
  for (const collectOnSettle of [true, false]) {
    const g = new Harness({ collectOnSettle });
    g.answer(10, { "mail.message": messages([7]) });
    const pending = g.store.beginRequest();
    expect(g.store.record("mail.message", 7).exists).toBe(true);
    g.store.settleRequest(pending);
    expect(g.store.record("mail.message", 7).exists).toBe(!collectOnSettle);
  }

  failsWith(() => h.store.settleRequest(request), "request", "request 1 is not outstanding");
  failsWith(() => h.store.settleRequest(99), "request", "request 99 is not outstanding");
  failsWith(() => h.store.applyAnswer(answerText(30, {}), 99), "request", "request 99 is not outstanding");
  const bare = new MessagingStore({ reportError: () => undefined });
  failsWith(() => bare.beginRequest(), "no_schema", "beginRequest: set a schema first");
  failsWith(() => bare.applyFrame(frameText(1, 1, {})), "no_schema", "applyFrame: set a schema first");
});

test("configuration is validated and has stated defaults", () => {
  const reportError = (): void => undefined;
  const make = (config: object) => () => new MessagingStore({ reportError, config: config as never });
  failsWith(() => new MessagingStore({} as never), "bad_config", "options.reportError is required: the store never swallows an error");
  failsWith(make({ pageSize: 0 }), "bad_config", "config.pageSize must be a positive integer, got 0");
  failsWith(make({ maxWindowPages: 1.5 }), "bad_config", "config.maxWindowPages must be a positive integer, got 1.5");
  failsWith(make({ maxThreads: -1 }), "bad_config", "config.maxThreads must be a positive integer, got -1");
  failsWith(make({ bogus: 1 }), "bad_config", "config.bogus is not a known option");
  failsWith(
    make({ collectModels: ["mail.message"] }),
    "bad_config",
    "config.collectModels must not contain the message model mail.message: windows own messages",
  );
  failsWith(make({ messageModel: "" }), "bad_config", "config.messageModel must be a non-empty string, got ");
  failsWith(make({ collectOnSettle: "yes" }), "bad_config", "config.collectOnSettle must be a boolean, got yes");

  const store = new MessagingStore({ reportError });
  expect(store.config).toEqual({
    pageSize: 30,
    maxWindowPages: 5,
    maxThreads: 50,
    messageModel: "mail.message",
    collectModels: ["res.partner", "ir.attachment"],
    collectOnSettle: true,
  });
  expect(store.windows.maxMessages).toBe(150);
});

test("leases are counted, released once, and refused for unknown or local models", () => {
  const h = new Harness({ maxThreads: 1 });
  const w = h.store.windows;
  const first = h.store.hold("mail.thread", 1);
  const second = h.store.hold("mail.thread", 1);
  for (const n of [1, 2, 3]) w.replace(thread(n), [n], false);
  for (const n of [1, 2, 3]) w.touch(thread(n));
  h.store.collect();
  expect(w.threads().map((t) => t.id).sort()).toEqual([1, 3]);

  first.release();
  first.release();
  w.touch(thread(3));
  h.store.collect();
  expect(w.get(thread(1))).toEqual({ ids: [1], hasOlder: false, hasNewer: false });

  second.release();
  w.touch(thread(3));
  h.store.collect();
  expect(w.get(thread(1))).toBeUndefined();
  expect(w.get(thread(3))).toBeDefined();

  h.store.defineLocalModel("ui.draft", ["text"]);
  failsWith(() => h.store.hold("no.such", 1), "unknown_model", "hold: no.such is neither a model of schema g1 nor a local model");
  failsWith(() => h.store.hold("ui.draft", 1), "unknown_model", "hold(ui.draft): local records are never collected, hold server records only");
  failsWith(() => h.store.hold("res.partner", "1" as never), "bad_id", "hold(res.partner): a server record id must be a safe integer, got 1");
});

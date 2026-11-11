/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Model-scoped keyed record tables (6).

import { expect, test } from "vitest";
import { KeyedRecordStore, UIError, effect, flushSync } from "../src/index.js";

function usageError(fn: () => unknown): UIError {
  try {
    fn();
  } catch (error) {
    expect(error).toBeInstanceOf(UIError);
    expect((error as UIError).code).toBe("usage");
    return error as UIError;
  }
  throw new Error("expected a UIError");
}

test("keyed tables keep equal ids of different models and numeric versus string ids apart", () => {
  const store = new KeyedRecordStore();
  const messages = store.table("mail.message");
  const partners = store.table("res.partner");
  messages.set(1, "body", "hello");
  partners.set(1, "name", "Ada");
  messages.set("1", "body", "local draft");

  expect(messages.get(1, "body")).toBe("hello");
  expect(messages.get("1", "body")).toBe("local draft");
  expect(partners.get(1, "name")).toBe("Ada");
  expect(partners.has(1, "body")).toBe(false);
  expect(messages.has(1, "name")).toBe(false);
  expect(messages.size).toBe(2);
  expect(partners.size).toBe(1);
  expect(store.table("mail.message")).toBe(messages);
  expect(store.peekTable("res.partner")).toBe(partners);
  expect(store.peekTable("mail.followers")).toBeUndefined();
  expect([...store.models()]).toEqual(["mail.message", "res.partner"]);
});

test("deleting a record removes only its own row and keeps its reader subscribed", () => {
  const store = new KeyedRecordStore();
  const messages = store.table("mail.message");
  const partners = store.table("res.partner");
  for (const id of [1, 2, 3]) {
    messages.set(id, "body", `m${id}`);
    messages.set(id, "author", id + 10);
    partners.set(id, "name", `p${id}`);
  }
  const seen: unknown[] = [];
  const stop = effect(() => {
    seen.push(messages.get(2, "body"));
  });
  flushSync();

  messages.deleteRecord(2);
  flushSync();

  expect(messages.has(2, "body")).toBe(false);
  expect(messages.has(2, "author")).toBe(false);
  expect(messages.hasRecord(2)).toBe(false);
  expect(messages.size).toBe(2);
  expect(new Set(messages.recordIds())).toEqual(new Set([1, 3]));
  expect(messages.get(1, "body")).toBe("m1");
  expect(messages.get(3, "author")).toBe(13);
  expect(partners.get(2, "name")).toBe("p2");
  expect(partners.size).toBe(3);

  messages.set(2, "body", "again");
  flushSync();
  expect(seen).toEqual(["m2", undefined, "again"]);
  expect(messages.hasRecord(2)).toBe(true);
  expect(new Set(messages.recordIds())).toEqual(new Set([1, 2, 3]));
  stop();

  messages.deleteRecord(99);
  expect(messages.size).toBe(3);

  messages.deleteField("author");
  expect(messages.has(1, "author")).toBe(false);
  expect(messages.get(1, "body")).toBe("m1");
  expect(partners.get(1, "name")).toBe("p1");
});

test("keyed tables allocate signals only for read coordinates and drop a deleted row after its last reader", () => {
  const store = new KeyedRecordStore();
  const partners = store.table("res.partner");
  for (let id = 1; id <= 500; id++) partners.set(id, "name", `Partner ${id}`);
  expect(partners.get(42, "name")).toBe("Partner 42");
  expect(partners.signalCount).toBe(0);

  let runs = 0;
  const stop = effect(() => {
    runs++;
    partners.get(42, "name");
  });
  flushSync();
  expect(runs).toBe(1);
  expect(partners.signalCount).toBe(1);

  partners.set(43, "name", "other");
  partners.set(42, "email", "a@example.com");
  flushSync();
  expect(runs).toBe(1);
  expect(partners.signalCount).toBe(1);

  expect(partners.rowCount).toBe(500);
  partners.deleteRecord(42);
  flushSync();
  expect(runs).toBe(2);
  expect(partners.signalCount).toBe(1);
  expect(partners.hasRecord(42)).toBe(false);
  // The emptied row stays while its reader is subscribed ...
  expect(partners.rowCount).toBe(500);

  stop();
  // ... and is released with the last signal.
  expect(partners.signalCount).toBe(0);
  expect(partners.rowCount).toBe(499);
  expect(partners.size).toBe(499);

  partners.deleteRecord(41);
  expect(partners.rowCount).toBe(498);
  partners.set(42, "name", "back");
  expect(partners.get(42, "name")).toBe("back");
  expect(partners.signalCount).toBe(0);
});

test("keyed tables refuse malformed model names and record ids", () => {
  const store = new KeyedRecordStore();
  expect(usageError(() => store.table("")).message).toBe(
    'KeyedRecordStore.table: model must be a non-empty string, got ""',
  );
  expect(usageError(() => store.table(7 as unknown as string)).message).toBe(
    "KeyedRecordStore.table: model must be a non-empty string, got 7",
  );
  expect(usageError(() => store.peekTable("")).message).toBe(
    'KeyedRecordStore.peekTable: model must be a non-empty string, got ""',
  );
  const t = store.table("res.partner");
  expect(usageError(() => t.set("", "name", "x")).message).toBe(
    'ModelTable(res.partner).set: record id must be a safe integer or a non-empty string, got ""',
  );
  expect(usageError(() => t.set(1.5, "name", "x")).message).toBe(
    "ModelTable(res.partner).set: record id must be a safe integer or a non-empty string, got 1.5",
  );
  expect(usageError(() => t.get(Number.NaN, "name")).message).toBe(
    "ModelTable(res.partner).get: record id must be a safe integer or a non-empty string, got NaN",
  );
  expect(usageError(() => t.has(2 ** 53, "name")).message).toBe(
    "ModelTable(res.partner).has: record id must be a safe integer or a non-empty string, got 9007199254740992",
  );
  expect(usageError(() => t.deleteRecord(null as unknown as number)).message).toBe(
    "ModelTable(res.partner).deleteRecord: record id must be a safe integer or a non-empty string, got null",
  );
  expect(usageError(() => t.hasRecord({} as unknown as number)).message).toBe(
    "ModelTable(res.partner).hasRecord: record id must be a safe integer or a non-empty string, got [object Object]",
  );
  expect(usageError(() => t.set(1, "", "x")).message).toBe(
    'ModelTable(res.partner).set: field must be a non-empty string, got ""',
  );
  expect(usageError(() => t.get(1, 3 as unknown as string)).message).toBe(
    "ModelTable(res.partner).get: field must be a non-empty string, got 3",
  );
  expect(usageError(() => t.deleteField(undefined as unknown as string)).message).toBe(
    "ModelTable(res.partner).deleteField: field must be a non-empty string, got undefined",
  );
  expect(usageError(() => t.set(1n as unknown as number, "name", "x")).message).toBe(
    "ModelTable(res.partner).set: record id must be a safe integer or a non-empty string, got 1n",
  );
  expect(usageError(() => t.get(Object.create(null) as number, "name")).message).toBe(
    "ModelTable(res.partner).get: record id must be a safe integer or a non-empty string, got [object Object]",
  );
  expect(t.size).toBe(0);
  expect(t.rowCount).toBe(0);
  expect([...store.models()]).toEqual(["res.partner"]);
});

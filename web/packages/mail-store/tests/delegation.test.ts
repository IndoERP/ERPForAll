/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Delegation from a channel to its thread (spec messaging-client-store.md).
import { effect, flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Harness, SCHEMA_G2, failsWith } from "./support";
import { MessagingStore, type RecordHandle } from "../src/index";

test("a channel delegates unknown fields to its thread", () => {
  const h = new Harness();
  h.store.delegate("discuss.channel", { via: "thread" });
  h.answer(10, {
    "discuss.channel": [{ id: 7, name: "general", thread: 70 }, { id: 8, name: "empty", thread: null }],
    "mail.thread": [
      { id: 70, name: "the thread's name", title: "General thread", unread: 4 },
      { id: 71, title: "Other thread" },
    ],
  });
  const channel = h.store.record("discuss.channel", 7);
  expect(channel.get("title")).toBe("General thread");
  expect(channel.counter("unread")).toEqual({ value: 4, saturated: false, stale: false });
  // The channel's own field wins over the thread's field of the same name.
  expect(channel.get("name")).toBe("general");
  // An empty many2one reads as nothing.
  expect(h.store.record("discuss.channel", 8).get("title")).toBeUndefined();
  failsWith(
    () => (channel as RecordHandle).get("nope"),
    "unknown_field",
    "discuss.channel.nope is not a field of discuss.channel or of mail.thread",
  );

  // A write never makes a silent local copy: not on a delegated field, not on the record's own.
  failsWith(
    () => channel.set("title", "x"),
    "read_only",
    "discuss.channel.title is read-only: it is delegated to mail.thread and server records are written by payloads only",
  );
  failsWith(() => channel.set("name", "x"), "read_only", "discuss.channel.name is read-only: server records are written by payloads only");
  expect(channel.get("title")).toBe("General thread");
  expect(channel.get("name")).toBe("general");

  const seen: unknown[] = [];
  const stop = effect(() => {
    seen.push(channel.get("title"));
  });
  flushSync();
  h.frame(20, 1, { "mail.thread": [{ id: 70, title: "Renamed" }] });
  flushSync();
  // Pointing the channel at another thread switches what it reads.
  h.frame(21, 2, { "discuss.channel": [{ id: 7, thread: 71 }] });
  flushSync();
  expect(seen).toEqual(["General thread", "Renamed", "Other thread"]);
  stop();
});

test("delegation rules are validated and a schema change keeps the old state when a rule breaks", () => {
  const bare = new MessagingStore({ reportError: () => undefined });
  failsWith(() => bare.delegate("discuss.channel", { via: "thread" }), "no_schema", "delegate(discuss.channel): set a schema first");

  const h = new Harness();
  failsWith(() => h.store.delegate("no.such", { via: "x" }), "unknown_model", "delegate(no.such): no.such is not a model of schema g1");
  const missing = "delegate(discuss.channel): nope must be a many2one field of discuss.channel in schema g1";
  failsWith(() => h.store.delegate("discuss.channel", { via: "nope" }), "delegation", missing);
  failsWith(
    () => h.store.delegate("discuss.channel", { via: "name" }),
    "delegation",
    "delegate(discuss.channel): name must be a many2one field of discuss.channel in schema g1",
  );
  h.store.delegate("discuss.channel", { via: "thread" });
  failsWith(() => h.store.delegate("discuss.channel", { via: "thread" }), "delegation", "discuss.channel already delegates through thread");
  failsWith(
    () => h.store.delegate("mail.thread", { via: "channel" }),
    "delegation",
    "delegating mail.thread through channel to discuss.channel would make a cycle",
  );

  h.answer(10, { "res.partner": [{ id: 1, name: "Ada" }] });
  failsWith(
    () => h.store.setSchema(SCHEMA_G2),
    "delegation",
    "delegate(discuss.channel): thread must be a many2one field of discuss.channel in schema g2",
  );
  expect(h.store.generation).toBe("g1");
  expect(h.store.record("res.partner", 1).get("name")).toBe("Ada");
});

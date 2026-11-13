/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The data contract with the store: required fields, capabilities that follow the schema, shape checks on values.
import { RegistryHub } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Thread, assertContract, capabilitiesOf, parseTracking, parseWireDatetime, readMessage, defineThreadViewRegistries } from "../src/index";
import { Rig, SCHEMA_BROKEN, SCHEMA_MIN, failsWith, message } from "./support";

test("a schema without a required message field refuses to mount a thread and names the field", () => {
  const rig = new Rig({ schema: SCHEMA_BROKEN, builtins: false });
  failsWith(() => assertContract(rig.store), "schema", "mail.message.date is not in schema t2: the thread view needs it");
  failsWith(() => new Thread(rig.threadOptions()), "schema", "mail.message.date is not in schema t2: the thread view needs it");
  // Nothing was held for the refused mount.
  expect(rig.errors).toEqual([]);
});

test("capabilities exist only where the schema declares their fields and follow a schema change", () => {
  const full = new Rig();
  expect(capabilitiesOf(full.store)).toEqual({
    subject: true, internal: true, parent: true, tracking: true, attachments: true, reactions: true,
    bookmark: true, pin: true, edited: true, partnerNames: true,
  });
  const minimal = new Rig({ schema: SCHEMA_MIN });
  expect(capabilitiesOf(minimal.store)).toEqual({
    subject: false, internal: false, parent: false, tracking: false, attachments: false, reactions: false,
    bookmark: false, pin: false, edited: false, partnerNames: true,
  });
  // The probe is cached per generation and recomputed when the generation changes.
  expect(capabilitiesOf(minimal.store)).toBe(capabilitiesOf(minimal.store));
  minimal.store.setSchema(JSON.stringify({ wire: 1, schema: "t3", models: { "mail.thread": { fields: { name: { kind: "string" } } }, "mail.message": { fields: { body: { kind: "html" }, message_type: { kind: "selection" }, date: { kind: "datetime" }, author_id: { kind: "int" }, starred: { kind: "bool" } } } } }));
  expect(capabilitiesOf(minimal.store)).toMatchObject({ bookmark: true, partnerNames: false, reactions: false });
});

test("a message is read as a plain snapshot with checked shapes and a loud error for a malformed value", () => {
  const rig = new Rig();
  rig.records({
    "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }],
    "ir.attachment": [{ id: 4, name: "plan.pdf", mimetype: "application/pdf" }],
    "mail.message.reaction": [{ id: 1, content: "\u{1F44D}", partner_id: 1 }, { id: 2, content: "\u{1F44D}", partner_id: 2 }, { id: 3, content: "\u{1F389}", partner_id: 1 }],
    "mail.message": [
      message(5, { subject: "Order 42", attachments: [4], reactions: [1, 2, 3], starred: true, pinned_at: "2026-11-08 10:00:00", edited_at: "2026-11-08 09:30:00", is_internal: true, parent_id: 4, tracking: JSON.stringify([{ f: 3, n: "stage_id", t: "many2one", o: 1, v: 2, od: "New", vd: "Won" }]) }),
      message(7, { tracking: "not json" }),
    ],
  });
  const data = readMessage(rig.store, 5, 2);
  expect(data).toMatchObject({
    id: 5, body: "<p>Message 5</p>", messageType: "comment", authorId: 1, subject: "Order 42", isInternal: true, parentId: 4, starred: true,
    attachments: [{ id: 4, name: "plan.pdf", mimetype: "application/pdf" }],
    reactions: [
      { content: "\u{1F44D}", count: 2, partnerIds: [1, 2], own: true },
      { content: "\u{1F389}", count: 1, partnerIds: [1], own: false },
    ],
    tracking: [{ field: "stage_id", type: "many2one", old: 1, new: 2, oldDisplay: "New", newDisplay: "Won" }],
  });
  expect(data?.date.toISOString()).toBe("2026-11-08T09:00:00.000Z");
  expect(data?.pinnedAt?.toISOString()).toBe("2026-11-08T10:00:00.000Z");
  expect(data?.editedAt?.toISOString()).toBe("2026-11-08T09:30:00.000Z");
  expect(readMessage(rig.store, 99, 2)).toBeUndefined();
  // The wire decoder already refuses a malformed datetime; the reader still checks what it is given.
  failsWith(() => parseWireDatetime("2026-02-30 10:00:00", "w"), "data", "w is not a calendar date: 2026-02-30 10:00:00");
  failsWith(() => parseWireDatetime("yesterday", "w"), "data", 'w must be a wire datetime (YYYY-MM-DD HH:MM:SS), got "yesterday"');
  failsWith(() => parseWireDatetime(5, "w"), "data", "w must be a wire datetime (YYYY-MM-DD HH:MM:SS), got 5");
  let caught: unknown;
  try {
    readMessage(rig.store, 7, 2);
  } catch (error) {
    caught = error;
  }
  expect(caught).toMatchObject({ name: "ThreadViewError", code: "data" });
  expect((caught as Error).message).toMatch(/^mail\.message#7\.tracking is not valid JSON: /);
});

test("tracking JSON is validated change by change with the exact path in the message", () => {
  failsWith(() => parseTracking("{}", "t"), "data", "t must be a JSON array of changes");
  failsWith(() => parseTracking("[1]", "t"), "data", "t[0] must be an object");
  failsWith(() => parseTracking('[{"t":"char"}]', "t"), "data", "t[0].n must be the field name");
  failsWith(() => parseTracking('[{"n":"x"}]', "t"), "data", "t[0].t must be the field type");
  failsWith(() => parseTracking('[{"n":"x","t":"char","od":3}]', "t"), "data", "t[0].od must be a string or null");
  failsWith(() => parseTracking(JSON.stringify(Array.from({ length: 257 }, () => ({ n: "x", t: "char" }))), "t"), "data", "t lists 257 changes; the limit is 256");
  expect(parseTracking('[{"n":"name","t":"char","o":"a","v":"b"}]', "t")).toEqual([{ field: "name", type: "char", old: "a", new: "b", oldDisplay: null, newDisplay: null }]);
});

test("the registries are defined once per hub and each guard refuses a malformed value", () => {
  const hub = new RegistryHub();
  const registries = defineThreadViewRegistries(hub);
  expect(() => defineThreadViewRegistries(hub)).toThrowError('Registry category "mail.messageActions" is already defined');
  const bad = { label: "x" } as never;
  expect(() => registries.messageActions.add("a", bad)).toThrowError('Invalid value for key "a" in the "mail.messageActions" registry');
  expect(() => registries.composerActions.add("a", bad)).toThrowError('Invalid value for key "a" in the "mail.composerActions" registry');
  expect(() => registries.messageRenderers.add("a", { match: () => true, render: () => document.createElement("p"), layout: "wide" } as never)).toThrowError(
    'Invalid value for key "a" in the "mail.messageRenderers" registry',
  );
  expect(() => registries.suggestionProviders.add("@", { delimiter: "@", label: () => "x" } as never)).toThrowError(
    'Invalid value for key "@" in the "mail.suggestionProviders" registry',
  );
  expect(() => registries.suggestionProviders.add("a b", { delimiter: "a b", label: () => "x", search: () => [] })).toThrowError(
    'Invalid value for key "a b" in the "mail.suggestionProviders" registry',
  );
  registries.messageActions.add("ok", { label: () => "Ok", run: () => undefined });
  expect(registries.messageActions.contains("ok")).toBe(true);
});

/// (partial: malformed reaction rows throw a data error instead of being dropped)
test("a reaction row without content or partner throws a data error instead of being dropped", () => {
  const rig = new Rig();
  rig.records({
    "res.partner": [{ id: 1, name: "Ann" }],
    "ir.attachment": [{ id: 6, name: "empty", mimetype: "" }],
    "mail.message.reaction": [{ id: 1, content: "", partner_id: 1 }, { id: 2, content: "\u{1F44D}", partner_id: null }],
    "mail.message": [message(10, { reactions: [1] }), message(11, { reactions: [2] }), message(14, { attachments: [6] })],
  });
  failsWith(() => readMessage(rig.store, 10, 1), "data", 'mail.message#10.reactions#1.content must be non-empty text, got ""');
  failsWith(() => readMessage(rig.store, 11, 1), "data", "mail.message#11.reactions#2.partner_id must be a partner id, got null");
  // An empty optional attachment value is empty, not an error.
  expect(readMessage(rig.store, 14, 1)?.attachments).toEqual([expect.objectContaining({ id: 6, mimetype: null })]);
});

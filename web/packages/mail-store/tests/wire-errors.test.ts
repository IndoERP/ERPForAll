/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Unknown names and generations after the schema fetch (spec messaging-client-store.md).
import { WireError } from "@erpforall/store-wire";
import { expect, test } from "vitest";
import { Harness, SCHEMA_G2, answerText, frameText } from "./support";

test("an unknown model is reported, nothing is applied and a resync is requested", () => {
  const h = new Harness();
  const records = { "mail.message": [{ id: 1, body: "valid" }], "no.such": [{ id: 1, x: 1 }] };
  const results = [h.store.applyAnswer(answerText(10, records)), h.store.applyFrame(frameText(20, 1, records))];
  const message = "unknown_model at records.no.such: unknown model no.such";
  for (const result of results) {
    expect(result).toMatchObject({
      status: "refused",
      resync: [{ reason: "unknown_model", path: "records.no.such", detail: message }],
    });
  }
  expect(h.errors).toHaveLength(2);
  for (const error of h.errors) {
    expect(error).toBeInstanceOf(WireError);
    expect(error).toMatchObject({ code: "unknown_model", path: "records.no.such", message });
  }
  // The valid record of the same payload was not applied either.
  expect(h.store.record("mail.message", 1).exists).toBe(false);
});

test("an unknown field is reported, nothing is applied and a resync is requested", () => {
  const h = new Harness();
  const result = h.frame(20, 1, { "mail.message": [{ id: 1, body: "valid", nope: 1 }] });
  const message = "unknown_field at records.mail.message[0].nope: unknown field nope";
  expect(result).toMatchObject({
    status: "refused",
    resync: [{ reason: "unknown_field", path: "records.mail.message[0].nope", detail: message }],
  });
  expect(h.errors).toHaveLength(1);
  expect(h.errors[0]).toBeInstanceOf(WireError);
  expect(h.errors[0]).toMatchObject({ code: "unknown_field", message });
  expect(h.store.record("mail.message", 1).exists).toBe(false);
  expect(h.store.record("mail.message", 1).get("body")).toBeUndefined();

  // Any other decoder refusal is reported too, with the generic reason.
  const bad = h.store.applyAnswer("{");
  expect(bad).toMatchObject({ status: "refused", resync: [{ reason: "payload_refused" }] });
  expect(h.errors).toHaveLength(2);
  expect(h.errors[1]).toBeInstanceOf(WireError);
  expect(h.errors[1]).toMatchObject({ code: "malformed", message: "malformed at <document>: expected a string key" });
});

test("a payload of another schema generation waits for its schema, which resets server records only", () => {
  const h = new Harness();
  h.answer(10, { "res.partner": [{ id: 1, name: "Ada" }, { id: 2, name: "Bo" }] });
  const thread = { model: "mail.thread", id: 1 };
  h.store.windows.replace(thread, [1, 2], false);
  const drafts = h.store.defineLocalModel("ui.draft", ["text", "thread"]);
  drafts.create("1", { text: "kept" });
  h.store.hold("res.partner", 1);
  const request = h.store.beginRequest();

  const g2 = answerText(20, { "res.partner": [{ id: 1, name: "New Ada" }, { id: 3, name: "Cy" }] }, [], "g2");
  expect(h.store.applyAnswer(g2)).toEqual({ status: "needs_schema", generation: "g2" });
  expect(h.store.record("res.partner", 1).get("name")).toBe("Ada");

  expect(h.store.setSchema(SCHEMA_G2)).toEqual({ changed: true, resync: [{ reason: "schema_changed" }] });
  expect(h.store.generation).toBe("g2");
  expect(h.store.record("res.partner", 1).exists).toBe(false);
  expect(h.store.windows.get(thread)).toBeUndefined();
  expect(drafts.get("1", "text")).toBe("kept");
  expect(h.store.setSchema(SCHEMA_G2)).toEqual({ changed: false, resync: [] });

  expect(h.store.applyAnswer(g2)).toMatchObject({ status: "applied" });
  expect(h.store.record("res.partner", 1).get("name")).toBe("New Ada");
  // The lease survived the reset: partner 1 stays, the unheld partner 3 is collected.
  expect(h.store.collect().evicted).toEqual({ "res.partner": 1 });
  expect(h.store.record("res.partner", 1).exists).toBe(true);
  expect(h.store.record("res.partner", 3).exists).toBe(false);
  // A request begun under the old generation settles without error.
  h.store.settleRequest(request);
});

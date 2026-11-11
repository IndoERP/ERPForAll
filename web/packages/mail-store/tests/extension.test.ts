/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// A test addon extends the messaging store through declarations, delegation, a local model and patch(), without
// changing the package (spec messaging-client-store.md, extension point proof).
import { patch } from "@erpforall/elang";
import { expect, test } from "vitest";
import { type MessagingStore, type One, RecordHandle } from "@erpforall/mail-store";
import { Harness, failsWith } from "./support";

declare module "@erpforall/mail-store" {
  interface StoreModels {
    // The addon's model; `name` is the owner partner's, read through delegation.
    "acme.note": { text: string; owner: One<"res.partner">; name: string };
    "acme.ui": { open: boolean };
  }
  // The addon's read helper, added to every handle by patch().
  interface RecordHandle<M extends string = string> {
    acmeLabel(): string;
  }
}

const ACME_SCHEMA = JSON.stringify({
  wire: 1,
  schema: "acme1",
  models: {
    "res.partner": { fields: { name: { kind: "string" } } },
    "acme.note": { fields: { text: { kind: "string" }, owner: { kind: "many2one", target: "res.partner" } } },
  },
});

/** The addon's registration: what an addon's client entry point does with the store. */
function registerAcme(store: MessagingStore): () => void {
  const unpatch = patch(RecordHandle, () => ({
    acmeLabel(): string {
      return `${this.model}#${String(this.id)}`;
    },
  }));
  store.delegate("acme.note", { via: "owner" });
  store.defineLocalModel("acme.ui", ["open"]);
  return unpatch;
}

test("an addon declares fields and models, delegates, adds a local model and patches the handle", () => {
  const h = new Harness({}, ACME_SCHEMA, "acme1");
  const unpatch = registerAcme(h.store);
  h.answer(10, { "acme.note": [{ id: 1, text: "call back", owner: 3 }], "res.partner": [{ id: 3, name: "Ada" }] });

  const note = h.store.record("acme.note", 1);
  const text: string | undefined = note.get("text");
  const viaDelegation: string | undefined = note.get("name");
  const label: string = note.acmeLabel();
  expect([text, viaDelegation, label]).toEqual(["call back", "Ada", "acme.note#1"]);
  expect(note.one("owner")?.get("name")).toBe("Ada");
  // @ts-expect-error `shout` is declared by no addon
  failsWith(() => note.get("shout"), "unknown_field", "acme.note.shout is not a field of acme.note or of res.partner");

  h.store.local("acme.ui").create("panel", { open: true });
  expect(h.store.record("acme.ui", "panel").get("open")).toBe(true);

  // Removing the patch restores the handle; the rest of the addon's registration is the addon's to undo.
  unpatch();
  expect((note as unknown as { acmeLabel?: () => string }).acmeLabel).toBeUndefined();
});

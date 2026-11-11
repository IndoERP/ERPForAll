/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shared helpers for the mail-store suites: a schema with the shapes the cases need, wire payload text builders and a
// store harness that records every reported error.
import { expect } from "vitest";
import type { WireError } from "@erpforall/store-wire";
import {
  type Counter,
  type Many,
  MessagingStore,
  MessagingStoreError,
  type MessagingStoreErrorCode,
  type MessagingConfig,
  type One,
  type ApplyResult,
} from "../src/index";

// Typed records for the test models: the same seam an addon uses.
declare module "@erpforall/mail-store" {
  interface StoreModels {
    "res.partner": { name: string };
    "ir.attachment": { name: string };
    "mail.message": { body: string; author: One<"res.partner">; attachments: Many<"ir.attachment"> };
    "mail.thread": { name: string; title: string; unread: Counter; capped: Counter; messages: Many<"mail.message">; channel: One<"discuss.channel"> };
    // `title` and `unread` are the thread's: the declaration lists the fields the channel reads through delegation.
    "discuss.channel": { name: string; thread: One<"mail.thread">; title: string; unread: Counter };
    "ui.draft": { text: string; thread: number };
  }
}

/** Schema generation `g1`: every field kind the cases need. */
export const SCHEMA_G1 = JSON.stringify({
  wire: 1,
  schema: "g1",
  models: {
    "res.partner": { fields: { name: { kind: "string" } } },
    "ir.attachment": { fields: { name: { kind: "string" } } },
    "mail.message": {
      fields: {
        body: { kind: "string" },
        author: { kind: "many2one", target: "res.partner" },
        attachments: { kind: "x2many", target: "ir.attachment" },
      },
    },
    "mail.thread": {
      fields: {
        name: { kind: "string" },
        title: { kind: "string" },
        channel: { kind: "many2one", target: "discuss.channel" },
        unread: { kind: "counter" },
        capped: { kind: "counter", cap: 5 },
        messages: { kind: "x2many", target: "mail.message" },
      },
    },
    "discuss.channel": { fields: { name: { kind: "string" }, thread: { kind: "many2one", target: "mail.thread" } } },
    "discuss.member": { fields: { partner: { kind: "many2one", target: "res.partner" } } },
  },
});

/** Generation `g2`: the same models without `discuss.channel.thread`, plus a new model. */
export const SCHEMA_G2 = JSON.stringify({
  wire: 1,
  schema: "g2",
  models: {
    "res.partner": { fields: { name: { kind: "string" } } },
    "discuss.channel": { fields: { name: { kind: "string" } } },
    "mail.extra": { fields: { note: { kind: "string" } } },
  },
});

type Records = Record<string, readonly unknown[]>;

/** An `asof` object with decimal-string xids. */
export function asof(xmin: number, xmax: number, xip: readonly number[] = [], own: number | null = null): unknown {
  return { xmin: String(xmin), xmax: String(xmax), xip: xip.map(String), own: own === null ? null : String(own) };
}

/** The text of an answer read at `xmax` (everything before it completed, `xip` still running). */
export function answerText(xmax: number, records: Records, xip: readonly number[] = [], schema = "g1"): string {
  return JSON.stringify({ wire: 1, schema, asof: asof(1, xmax, xip), records });
}

/** The text of the bus frame of transaction `xid` (its snapshot knows every earlier transaction). */
export function frameText(xid: number, id: number, records: Records, schema = "g1"): string {
  return JSON.stringify({
    wire: 1,
    schema,
    cursor: { xid: String(xid), id: String(id) },
    asof: asof(1, xid, [], xid),
    records,
  });
}

/** A store with the `g1` schema that collects every reported error. */
export class Harness {
  readonly errors: (MessagingStoreError | WireError)[] = [];
  readonly store: MessagingStore;

  readonly generation: string;

  constructor(config: Partial<MessagingConfig> = {}, schema: string | undefined = SCHEMA_G1, generation = "g1") {
    this.generation = generation;
    this.store = new MessagingStore({
      reportError: (error) => this.errors.push(error),
      config: { collectOnSettle: false, ...config },
      ...(schema === undefined ? {} : { schema }),
    });
  }

  answer(xmax: number, records: Records, request?: number, xip: readonly number[] = []): ApplyResult {
    return this.store.applyAnswer(answerText(xmax, records, xip, this.generation), request);
  }

  frame(xid: number, id: number, records: Records): ApplyResult {
    return this.store.applyFrame(frameText(xid, id, records, this.generation));
  }
}

/** Runs `fn` and asserts it throws a `MessagingStoreError` with this code and exact message. */
export function failsWith(fn: () => unknown, code: MessagingStoreErrorCode, message: string): void {
  let error: unknown;
  try {
    fn();
  } catch (caught) {
    error = caught;
  }
  expect(error).toBeInstanceOf(MessagingStoreError);
  expect(error).toMatchObject({ name: "MessagingStoreError", code, message });
}

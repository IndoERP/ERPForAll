/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shared helpers for the thread view suites: a schema with every field the contract names, wire payload builders, a
// fake transport and pager, and a rig that builds components over a real messaging store.
import { RegistryHub, createTranslator, flushSync } from "@erpforall/elang";
import { MessagingStore } from "@erpforall/mail-store";
import { afterEach, expect } from "vitest";
import {
  Composer,
  Thread,
  ThreadViewError,
  defineThreadViewRegistries,
  registerBuiltins,
  type ComposerOptions,
  type ThreadOptions,
  type ThreadPager,
  type ThreadPostAnswer,
  type ThreadPostDto,
  type ThreadTransport,
  type ThreadViewErrorCode,
  type ThreadViewRegistries,
  type ThreadViewServices,
  type OperationAnswer,
} from "../src/index";

const field = (kind: string, extra: Record<string, unknown> = {}): Record<string, unknown> => ({ kind, ...extra });

/** Schema `t1`: the required message fields and every optional capability. */
export const SCHEMA = JSON.stringify({
  wire: 1,
  schema: "t1",
  models: {
    "res.partner": { fields: { name: field("string") } },
    "ir.attachment": { fields: { name: field("string"), mimetype: field("string"), file_size: field("int"), checksum: field("string"), type: field("selection"), url: field("string") } },
    "mail.message.reaction": { fields: { content: field("string"), partner_id: field("many2one", { target: "res.partner" }) } },
    "mail.thread": { fields: { name: field("string") } },
    "mail.message": {
      fields: {
        body: field("html"),
        subject: field("string"),
        message_type: field("selection"),
        date: field("datetime"),
        author_id: field("many2one", { target: "res.partner" }),
        parent_id: field("many2one", { target: "mail.message" }),
        is_internal: field("bool"),
        tracking: field("string"),
        attachments: field("x2many", { target: "ir.attachment" }),
        reactions: field("x2many", { target: "mail.message.reaction" }),
        starred: field("bool"),
        pinned_at: field("datetime"),
        edited_at: field("datetime"),
      },
    },
  },
});

/** Schema `t0`: only the required message fields (no capability is present). */
export const SCHEMA_MIN = JSON.stringify({
  wire: 1,
  schema: "t0",
  models: {
    "mail.thread": { fields: { name: field("string") } },
    "mail.message": {
      fields: {
        body: field("html"),
        message_type: field("selection"),
        date: field("datetime"),
        author_id: field("many2one", { target: "res.partner" }),
      },
    },
    "res.partner": { fields: { name: field("string") } },
  },
});

/** Schema `t2`: the message model without `date`. */
export const SCHEMA_BROKEN = JSON.stringify({
  wire: 1,
  schema: "t2",
  models: { "mail.thread": { fields: { name: field("string") } }, "mail.message": { fields: { body: field("html"), message_type: field("selection"), author_id: field("many2one", { target: "res.partner" }) } }, "res.partner": { fields: { name: field("string") } } },
});

type Records = Record<string, readonly unknown[]>;

/** The text of an answer read at `xmax`. */
export function answerText(xmax: number, records: Records, schema = "t1"): string {
  return JSON.stringify({ wire: 1, schema, asof: { xmin: "1", xmax: String(xmax), xip: [], own: null }, records });
}

/** A message record with sensible defaults. */
export function message(id: number, over: Record<string, unknown> = {}): Record<string, unknown> {
  return { id, body: `<p>Message ${id}</p>`, message_type: "comment", date: "2026-11-08 09:00:00", author_id: 1, ...over };
}

/** A transport that records its calls and answers from a handler the test replaces. */
export class FakeTransport implements ThreadTransport {
  readonly posts: { readonly ref: { model: string; id: number }; readonly dto: ThreadPostDto }[] = [];
  postHandler: (dto: ThreadPostDto) => Promise<ThreadPostAnswer> = () => Promise.reject(new Error("no post handler set"));
  readonly calls: { readonly name: string; readonly args: readonly unknown[] }[] = [];
  setReaction?: (messageId: number, content: string, present: boolean) => Promise<OperationAnswer>;
  setBookmark?: (messageId: number, on: boolean) => Promise<OperationAnswer>;
  setPinned?: (messageId: number, on: boolean) => Promise<OperationAnswer>;
  deleteMessage?: (messageId: number) => Promise<OperationAnswer>;

  post(ref: { model: string; id: number }, dto: ThreadPostDto): Promise<ThreadPostAnswer> {
    this.posts.push({ ref, dto });
    return this.postHandler(dto);
  }

  /** Install an operation that records its call and answers with `answer`. */
  enable(name: "setReaction" | "setBookmark" | "setPinned" | "deleteMessage", answer: OperationAnswer = {}): void {
    (this as Record<string, unknown>)[name] = (...args: unknown[]) => {
      this.calls.push({ name, args });
      return Promise.resolve(answer);
    };
  }
}

/** A pager that records calls; its handlers install pages through the rig. */
export class FakePager implements ThreadPager {
  readonly latest: { model: string; id: number }[] = [];
  readonly older: { ref: { model: string; id: number }; before: number }[] = [];
  latestHandler: () => Promise<void> = () => Promise.resolve();
  olderHandler: (before: number) => Promise<void> = () => Promise.resolve();
  loadLatest(ref: { model: string; id: number }): Promise<void> {
    this.latest.push(ref);
    return this.latestHandler();
  }
  loadOlder(ref: { model: string; id: number }, before: number): Promise<void> {
    this.older.push({ ref, before });
    return this.olderHandler(before);
  }
}

export const NOW = new Date("2026-11-08T12:00:00Z");

const live: { dispose(): void }[] = [];
afterEach(() => {
  for (const item of live.splice(0).reverse()) item.dispose();
  document.body.replaceChildren();
});

/** A store, registries, fakes and builders for the components under test. */
export class Rig {
  readonly errors: unknown[] = [];
  readonly store: MessagingStore;
  readonly hub: RegistryHub;
  readonly registries: ThreadViewRegistries;
  readonly transport = new FakeTransport();
  readonly pager = new FakePager();
  readonly ref = { model: "mail.thread", id: 7 };
  readonly translate = createTranslator({ language: "en_US" })._t;
  services: ThreadViewServices = { now: () => NOW, timeZone: "UTC", locale: "en-US", newUid: (() => { let n = 0; return () => `uid-${++n}`; })() };
  #xmax = 10;

  readonly generation: string;

  constructor(options: { schema?: string; hub?: RegistryHub; builtins?: boolean; actions?: boolean; config?: ConstructorParameters<typeof MessagingStore>[0]["config"] } = {}) {
    this.generation = (JSON.parse(options.schema ?? SCHEMA) as { schema: string }).schema;
    this.store = new MessagingStore({
      reportError: (error) => this.errors.push(error),
      config: { collectOnSettle: false, ...options.config },
      schema: options.schema ?? SCHEMA,
    });
    this.hub = options.hub ?? new RegistryHub();
    this.registries = defineThreadViewRegistries(this.hub);
    if (options.builtins !== false) registerBuiltins(this.registries);
    if (options.actions === false) {
      for (const [id] of this.registries.messageActions.getEntries()) this.registries.messageActions.remove(id);
      for (const [id] of this.registries.composerActions.getEntries()) this.registries.composerActions.remove(id);
    }
  }

  /** Apply records as one answer. */
  records(records: Records, schema = this.generation): void {
    this.#xmax += 10;
    const result = this.store.applyAnswer(answerText(this.#xmax, records, schema));
    expect(result.status).toBe("applied");
    flushSync();
  }

  /** The text of an answer for records, to hand to a transport (read after everything applied so far). */
  payload(records: Records, schema = this.generation): string {
    this.#xmax += 10;
    return answerText(this.#xmax, records, schema);
  }

  /** Install the latest page of the thread: the records and then the window. */
  load(messages: readonly Record<string, unknown>[], hasOlder = false, extra: Records = {}): void {
    this.records({ "mail.message": messages, ...extra });
    this.store.windows.replace(this.ref, messages.map((m) => m["id"] as number), hasOlder);
    flushSync();
  }

  report = (error: unknown): void => {
    this.errors.push(error);
  };

  threadOptions(extra: Partial<ThreadOptions> = {}): ThreadOptions {
    return {
      store: this.store,
      ref: this.ref,
      registries: this.registries,
      transport: this.transport,
      pager: this.pager,
      self: { partnerId: 2 },
      translate: this.translate,
      reportError: this.report,
      services: this.services,
      ...extra,
    };
  }

  composerOptions(extra: Partial<ComposerOptions> = {}): ComposerOptions {
    return { ...this.threadOptions(), platform: "other", ...extra } as ComposerOptions;
  }

  thread(extra: Partial<ThreadOptions> = {}): Thread {
    const thread = new Thread(this.threadOptions(extra));
    document.body.append(thread.element);
    live.push(thread);
    flushSync();
    return thread;
  }

  composer(extra: Partial<ComposerOptions> = {}): Composer {
    const composer = new Composer(this.composerOptions(extra));
    document.body.append(composer.element);
    live.push(composer);
    flushSync();
    return composer;
  }
}

/** Runs `fn` and asserts it throws a `ThreadViewError` with this code and exact message. */
export function failsWith(fn: () => unknown, code: ThreadViewErrorCode, message: string): void {
  let error: unknown;
  try {
    fn();
  } catch (caught) {
    error = caught;
  }
  expect(error).toBeInstanceOf(ThreadViewError);
  expect(error).toMatchObject({ name: "ThreadViewError", code, message });
}

/** Let promises and microtasks settle, then flush effects. */
export async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
  flushSync();
}

/** Type text into a field as a user does (one input event). */
export function type(field: HTMLTextAreaElement, text: string, caret = text.length): void {
  field.value = text;
  field.setSelectionRange(caret, caret);
  field.dispatchEvent(new Event("input", { bubbles: true }));
  flushSync();
}

/** Dispatch a keydown and report whether it was left unhandled. */
export function key(target: Element, name: string, init: KeyboardEventInit = {}): KeyboardEvent {
  const event = new KeyboardEvent("keydown", { key: name, bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  flushSync();
  return event;
}

/** The article element of a message. */
export function article(thread: Thread, id: number): HTMLElement {
  const row = thread.message(id);
  if (row === undefined) throw new Error(`message ${id} is not in the thread`);
  return row.element;
}

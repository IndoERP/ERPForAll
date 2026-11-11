/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Local UI records of a thread (3): the composer draft, the reply target and the unsent posts live in the
// messaging store as local records, never sent to the server, so the Thread and the Composer (which a host mounts
// separately) share them and a surface that is rebuilt keeps its draft.
import type { LocalModel, MessagingStore, ThreadRef } from "@erpforall/mail-store";
import { MessagingStoreError } from "@erpforall/mail-store";
import type { MentionRange } from "./mentions";

/** The per-thread UI record: draft text, kind, mentions, reply target and the uids of unsent posts. */
export const THREAD_UI_MODEL = "mail_ui.thread";
/** One unsent post (sending or failed). */
export const PENDING_MODEL = "mail_ui.pending";

/** The most unsent posts one thread keeps. */
export const MAX_PENDING = 20;

/** The composer kind: "Send message" or "Log note". */
export type ComposerKind = "message" | "note";

/** State of one unsent post. */
export type PendingState = "sending" | "failed";

/** The local models of a store, created on first use. */
export interface UiModels {
  readonly thread: LocalModel;
  readonly pending: LocalModel;
}

const defined = new WeakMap<MessagingStore, UiModels>();

/**
 * The local models, defined once per store.
 *
 * @param store - the messaging store
 */
export function uiModels(store: MessagingStore): UiModels {
  const known = defined.get(store);
  if (known !== undefined) return known;
  const models: UiModels = {
    thread: defineOrGet(store, THREAD_UI_MODEL, ["text", "kind", "mentions", "reply_to", "pending"]),
    pending: defineOrGet(store, PENDING_MODEL, ["thread", "text", "kind", "mentions", "parent_id", "extras", "client_uid", "state", "error"]),
  };
  defined.set(store, models);
  return models;
}

function defineOrGet(store: MessagingStore, model: string, fields: readonly string[]): LocalModel {
  try {
    return store.local(model);
  } catch (error) {
    if (error instanceof MessagingStoreError && error.code === "unknown_model") return store.defineLocalModel(model, fields);
    throw error;
  }
}

/** The id of a thread's UI record. */
export function threadKey(ref: ThreadRef): string {
  return `${ref.model}:${ref.id}`;
}

/**
 * The thread's UI record, created with an empty draft when missing.
 *
 * @param store - the messaging store
 * @param ref - the thread
 */
export function threadState(store: MessagingStore, ref: ThreadRef): { readonly model: LocalModel; readonly id: string } {
  const { thread } = uiModels(store);
  const id = threadKey(ref);
  if (!thread.has(id)) {
    thread.create(id, { text: "", kind: "message", mentions: [], reply_to: null, pending: [] });
  }
  return { model: thread, id };
}

/** The draft's mention list as the record holds it. */
export function draftMentions(value: unknown): readonly MentionRange[] {
  return Array.isArray(value) ? (value as readonly MentionRange[]) : [];
}

/** The uids of the thread's unsent posts, tracked. */
export function pendingUids(store: MessagingStore, ref: ThreadRef): readonly string[] {
  const { model, id } = threadState(store, ref);
  const value = model.get(id, "pending");
  return Array.isArray(value) ? (value as readonly string[]) : [];
}

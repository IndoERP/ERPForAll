/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// What the components need from the server, as typed interfaces matching the JSON-2 contract of the mail addon
// (thread_post: on wip/). The components never call the network: the host
// supplies a ThreadTransport and a ThreadPager.
import type { MessagingStore, ThreadRef } from "@erpforall/mail-store";
import { ThreadViewError, OWNING_TASK, unavailable } from "./errors";
import type { MentionSpan } from "./mentions";

/** "Send message" is subtype `comment`, "Log note" is `note`. */
export type PostSubtype = "comment" | "note";

/** The closed `post` object of `thread_post`. Any other key is refused by the server. */
export interface ThreadPostDto {
  /** Plain text; the server escapes it and renders the mentions as anchors. HTML is refused. */
  readonly body: string;
  readonly subject?: string;
  readonly message_type: "comment" | "notification";
  readonly subtype: PostSubtype;
  readonly parent_id?: number;
  readonly partner_ids?: readonly number[];
  readonly attachment_ids?: readonly number[];
  readonly mentions?: readonly MentionSpan[];
  readonly silent?: boolean;
  /** A UUID; a retry with the same value returns the stored message (`replayed`). */
  readonly client_uid: string;
}

/** The answer of `thread_post`. `store` is the store payload exactly as received (text), for `applyAnswer`. */
export interface ThreadPostAnswer {
  readonly message_id: number;
  readonly replayed: boolean;
  readonly notified_partner_ids?: readonly number[];
  readonly store: string | Uint8Array;
}

/** The answer of a message operation: the records it touched, as a store payload (text). */
export interface OperationAnswer {
  readonly store?: string | Uint8Array;
}

/** Message operations. Each exists only once the server method does; a host omits those it lacks. */
export interface MessageOperations {
  /** `set_reaction`: add (`present` true) or remove the viewer's reaction. */
  setReaction?(messageId: number, content: string, present: boolean): Promise<OperationAnswer>;
  setBookmark?(messageId: number, on: boolean): Promise<OperationAnswer>;
  /** `set_pinned`. */
  setPinned?(messageId: number, on: boolean): Promise<OperationAnswer>;
  /** Delete the message (the same rule as editing: own plain comments; the server decides). */
  deleteMessage?(messageId: number): Promise<OperationAnswer>;
}

/** What the composer and the message actions call. */
export interface ThreadTransport extends MessageOperations {
  /** `thread_post` on the thread's model. */
  post(ref: ThreadRef, post: ThreadPostDto): Promise<ThreadPostAnswer>;
}

/** The host's page loading (the `thread` and `thread.messages` store_fetch topics). */
export interface ThreadPager {
  /**
   * Fetch the latest page, apply its payload and call `store.windows.replace`.
   *
   * @param ref - the thread
   */
  loadLatest(ref: ThreadRef): Promise<void>;
  /**
   * Fetch the page of messages older than `before`, apply it and call `store.windows.prependOlder`.
   *
   * @param ref - the thread
   * @param before - the oldest message id of the window
   */
  loadOlder(ref: ThreadRef, before: number): Promise<void>;
}

/** The owner task named for each operation when the host's transport lacks it. */
const OPERATION_TASKS = {
  setReaction: ["Reacting to a message", OWNING_TASK.reactions],
  setBookmark: ["Bookmarking a message", OWNING_TASK.bookmark],
  setPinned: ["Pinning a message", OWNING_TASK.pin],
  deleteMessage: ["Deleting a message", OWNING_TASK.delete],
} as const;

/**
 * Return a transport operation or refuse loudly.
 *
 * @param transport - the host's transport
 * @param name - the operation
 * @throws {@link ThreadViewError} `unavailable` naming the owning task
 */
export function requireOperation<K extends keyof typeof OPERATION_TASKS>(
  transport: ThreadTransport,
  name: K,
): NonNullable<MessageOperations[K]> {
  const operation = transport[name];
  if (typeof operation !== "function") {
    const [what, task] = OPERATION_TASKS[name];
    throw unavailable(what, `${task} (the transport has no ${name} operation)`);
  }
  return (operation as (...args: never[]) => unknown).bind(transport) as NonNullable<MessageOperations[K]>;
}

/**
 * Check a `thread_post` answer against its contract.
 *
 * @param answer - what the transport returned
 * @throws {@link ThreadViewError} `transport` for a missing or mistyped member
 */
export function checkPostAnswer(answer: unknown): ThreadPostAnswer {
  if (answer === null || typeof answer !== "object") throw new ThreadViewError("transport", "thread_post answered with something that is not an object");
  const a = answer as Record<string, unknown>;
  if (typeof a["message_id"] !== "number" || !Number.isSafeInteger(a["message_id"]) || a["message_id"] < 1) {
    throw new ThreadViewError("transport", `thread_post answer.message_id must be a positive integer, got ${JSON.stringify(a["message_id"])}`);
  }
  if (typeof a["replayed"] !== "boolean") {
    throw new ThreadViewError("transport", `thread_post answer.replayed must be a boolean, got ${JSON.stringify(a["replayed"])}`);
  }
  if (typeof a["store"] !== "string" && !(a["store"] instanceof Uint8Array)) {
    throw new ThreadViewError("transport", "thread_post answer.store must be the store payload text");
  }
  return answer as ThreadPostAnswer;
}

/**
 * Apply a store payload as one request: `beginRequest`, `applyAnswer`, the caller's window update, `settleRequest`
 * (D-6). The window update runs before the request settles because settling collects, and a record the
 * answer brought is unreferenced until a window names it.
 *
 * @param store - the messaging store
 * @param payload - the payload text
 * @param afterApply - runs only when the payload was applied; place the answer's messages in their windows here
 * @returns the apply result; a refused or `needs_schema` result is returned, not hidden
 */
export function applyStorePayload(
  store: MessagingStore,
  payload: string | Uint8Array,
  afterApply?: () => void,
): ReturnType<MessagingStore["applyAnswer"]> {
  const request = store.beginRequest();
  try {
    const result = store.applyAnswer(payload, request);
    if (result.status === "applied") afterApply?.();
    return result;
  } finally {
    store.settleRequest(request);
  }
}

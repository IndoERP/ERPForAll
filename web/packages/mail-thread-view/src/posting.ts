/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Sending a post: the closed thread_post DTO, the unsent-post local record (optimistic row, Retry, Discard) and the
// placement of the answer in the thread's window (and 6.3).
import { batch } from "@erpforall/elang";
import { ThreadViewError } from "./errors";
import { MAX_BODY_BYTES, type MentionRange, toSpans, utf8Length } from "./mentions";
import type { ThreadViewContext } from "./registries";
import { MAX_PENDING, type ComposerKind, type PendingState, pendingUids, threadKey, threadState, uiModels } from "./state";
import { applyStorePayload, checkPostAnswer, type ThreadPostDto } from "./transport";
import { t } from "./context";

/** What the composer hands to {@link submitPost}. */
export interface PostInput {
  readonly text: string;
  readonly mentions: readonly MentionRange[];
  readonly kind: ComposerKind;
  readonly replyTo: number | null;
  readonly subject?: string;
  readonly partnerIds?: readonly number[];
  readonly attachmentIds?: readonly number[];
  readonly silent?: boolean;
}

/**
 * Build the closed `thread_post` object for a draft.
 *
 * @param input - the draft
 * @param uid - the `client_uid` of the post
 * @throws {@link ThreadViewError} `body` for an empty draft or one over 1 MiB of UTF-8; `mention` for an invalid span
 */
export function buildPostDto(input: PostInput, uid: string): ThreadPostDto {
  const hasAttachments = input.attachmentIds !== undefined && input.attachmentIds.length > 0;
  if (input.text.trim() === "" && !hasAttachments) throw new ThreadViewError("body", "a message needs text: the draft is empty");
  const lone = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/.exec(input.text);
  if (lone !== null) throw new ThreadViewError("body", `the draft has a lone surrogate (half of an emoji) at index ${lone.index}: remove it before sending`);
  const bytes = utf8Length(input.text);
  if (bytes > MAX_BODY_BYTES) throw new ThreadViewError("body", `the draft is ${bytes} bytes; a post holds at most ${MAX_BODY_BYTES}`);
  const mentions = toSpans(input.text, input.mentions);
  return {
    body: input.text,
    message_type: "comment",
    subtype: input.kind === "note" ? "note" : "comment",
    ...(input.subject !== undefined && input.subject !== "" ? { subject: input.subject } : {}),
    ...(input.replyTo !== null ? { parent_id: input.replyTo } : {}),
    ...(input.partnerIds !== undefined && input.partnerIds.length > 0 ? { partner_ids: [...input.partnerIds] } : {}),
    ...(input.attachmentIds !== undefined && input.attachmentIds.length > 0 ? { attachment_ids: [...input.attachmentIds] } : {}),
    ...(mentions.length > 0 ? { mentions } : {}),
    ...(input.silent === true ? { silent: true } : {}),
    client_uid: uid,
  };
}

function newUid(ctx: ThreadViewContext): string {
  const make = ctx.services.newUid ?? (typeof globalThis.crypto?.randomUUID === "function" ? () => globalThis.crypto.randomUUID() : undefined);
  if (make === undefined) throw new ThreadViewError("bad_option", "services.newUid is required: crypto.randomUUID does not exist in this environment");
  const uid = make();
  if (typeof uid !== "string" || uid === "") throw new ThreadViewError("bad_option", `services.newUid must return a non-empty string, got ${JSON.stringify(uid)}`);
  return uid;
}

/**
 * Create the unsent-post record for a draft and send it.
 *
 * @param ctx - the thread's context
 * @param input - the draft
 * @param onQueued - called once the unsent-post record exists (the composer clears its draft here)
 * @returns `sent` once the message is in the thread's window, `failed` when the post is left as a failed row
 * @throws {@link ThreadViewError} before anything is created: `body`, `mention`, `pending_limit`
 */
export async function submitPost(ctx: ThreadViewContext, input: PostInput, onQueued?: () => void): Promise<"sent" | "failed"> {
  const uid = newUid(ctx);
  buildPostDto(input, uid);
  const { pending } = uiModels(ctx.store);
  const { model, id } = threadState(ctx.store, ctx.ref);
  const list = pendingUids(ctx.store, ctx.ref);
  if (list.length >= MAX_PENDING) {
    throw new ThreadViewError("pending_limit", `${threadKey(ctx.ref)} already has ${list.length} unsent posts; the limit is ${MAX_PENDING}: retry or discard some first`);
  }
  batch(() => {
    pending.create(uid, {
      thread: threadKey(ctx.ref),
      text: input.text,
      kind: input.kind,
      mentions: [...input.mentions],
      parent_id: input.replyTo,
      extras: {
        subject: input.subject ?? null,
        partnerIds: input.partnerIds === undefined ? null : [...input.partnerIds],
        attachmentIds: input.attachmentIds === undefined ? null : [...input.attachmentIds],
        silent: input.silent === true,
      },
      client_uid: uid,
      state: "sending" satisfies PendingState,
      error: null,
    });
    model.set(id, { pending: [...list, uid] });
  });
  onQueued?.();
  return sendPending(ctx, uid);
}

const inflight = new Map<string, Promise<"sent" | "failed">>();

/**
 * Send (or resend with the same `client_uid`) an unsent post. Failures are shown, announced and reported, and leave
 * the record as a failed row; they are not thrown.
 *
 * @param ctx - the thread's context
 * @param uid - the unsent post's uid
 */
export function sendPending(ctx: ThreadViewContext, uid: string): Promise<"sent" | "failed"> {
  // A second activation (a double click on Retry) joins the send that is already running.
  const running = inflight.get(uid);
  if (running !== undefined) return running;
  const work = sendNow(ctx, uid).finally(() => inflight.delete(uid));
  inflight.set(uid, work);
  return work;
}

async function sendNow(ctx: ThreadViewContext, uid: string): Promise<"sent" | "failed"> {
  const { pending } = uiModels(ctx.store);
  if (!pending.has(uid)) throw new ThreadViewError("bad_option", `there is no unsent post ${uid}`);
  const extras = pending.get(uid, "extras") as {
    subject: string | null;
    partnerIds: readonly number[] | null;
    attachmentIds?: readonly number[] | null;
    silent: boolean;
  };
  const input: PostInput = {
    text: String(pending.get(uid, "text")),
    mentions: pending.get(uid, "mentions") as readonly MentionRange[],
    kind: pending.get(uid, "kind") as ComposerKind,
    replyTo: (pending.get(uid, "parent_id") as number | null) ?? null,
    ...(extras.subject !== null ? { subject: extras.subject } : {}),
    ...(extras.partnerIds !== null ? { partnerIds: extras.partnerIds } : {}),
    ...(extras.attachmentIds ? { attachmentIds: extras.attachmentIds } : {}),
    ...(extras.silent ? { silent: true } : {}),
  };
  pending.set(uid, { state: "sending", error: null });
  ctx.announce(t(ctx, "Sending"));
  try {
    const dto = buildPostDto(input, uid);
    const answer = checkPostAnswer(await ctx.transport.post(ctx.ref, dto));
    const placed = { reload: false };
    const result = applyStorePayload(ctx.store, answer.store, () => {
      const outcome = ctx.store.windows.appendLive(ctx.ref, answer.message_id);
      if (outcome === "gap" || outcome === "older" || outcome === "no_window") placed.reload = true;
    });
    if (result.status !== "applied") {
      throw new ThreadViewError(
        "transport",
        result.status === "needs_schema"
          ? `the server accepted the post but answered with schema ${result.generation}, which this tab does not hold: retry after the schema is fetched`
          : `the server accepted the post but its answer was refused (${result.error.message}): retry to fetch it again`,
      );
    }
    if (placed.reload) await ctx.pager.loadLatest(ctx.ref);
    removePending(ctx, uid);
    ctx.announce(t(ctx, "Message sent"));
    return "sent";
  } catch (error) {
    if (pending.has(uid)) pending.set(uid, { state: "failed" satisfies PendingState, error: error instanceof Error ? error.message : String(error) });
    ctx.announce(t(ctx, "The message was not sent"), "assertive");
    ctx.reportError(error);
    return "failed";
  }
}

function removePending(ctx: ThreadViewContext, uid: string): void {
  const { pending } = uiModels(ctx.store);
  const { model, id } = threadState(ctx.store, ctx.ref);
  batch(() => {
    const list = pendingUids(ctx.store, ctx.ref);
    model.set(id, { pending: list.filter((u) => u !== uid) });
    if (pending.has(uid)) pending.delete(uid);
  });
}

/**
 * Drop an unsent post (the Discard button). Its text is gone; the user chose that.
 *
 * @param ctx - the thread's context
 * @param uid - the unsent post's uid
 */
export function discardPending(ctx: ThreadViewContext, uid: string): void {
  removePending(ctx, uid);
  ctx.announce(t(ctx, "Unsent message discarded"));
}

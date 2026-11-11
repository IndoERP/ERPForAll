/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The data contract between the thread view and the messaging store.
// This file is the only place that names models and fields; reads go through dynamically typed handles until the
// Rust generator emits StoreModels entries for the mail schema.
import { untrack } from "@erpforall/elang";
import { MessagingStoreError, type MessagingStore, type RecordHandle } from "@erpforall/mail-store";
import { ThreadViewError } from "./errors";

/** The partner model the author and reaction fields point at. */
export const PARTNER_MODEL: string = "res.partner";
/** The reaction model (one row per message, partner and emoji: communication.md section 4.5). */
export const REACTION_MODEL: string = "mail.message.reaction";
/** The attachment model. */
export const ATTACHMENT_MODEL: string = "ir.attachment";

/** The fields every message must have in the schema; a schema without one makes a thread refuse to mount. */
export const REQUIRED_MESSAGE_FIELDS = Object.freeze(["body", "message_type", "date", "author_id"] as const);

/** The optional capabilities of the message model: each exists only when the schema declares its field. */
export interface Capabilities {
  readonly subject: boolean;
  readonly internal: boolean;
  readonly parent: boolean;
  readonly tracking: boolean;
  readonly attachments: boolean;
  readonly reactions: boolean;
  readonly bookmark: boolean;
  readonly pin: boolean;
  readonly edited: boolean;
  /** `res.partner.name` is in the schema, so author names can be shown. */
  readonly partnerNames: boolean;
}

/** One change of a tracked field (communication.md section 4.3: `{f, n, t, o, v, od, vd, cur}`). */
export interface TrackingChange {
  readonly field: string;
  readonly type: string;
  readonly old: unknown;
  readonly new: unknown;
  readonly oldDisplay: string | null;
  readonly newDisplay: string | null;
}

/** One emoji with the partners that used it. */
export interface ReactionGroup {
  readonly content: string;
  readonly count: number;
  readonly partnerIds: readonly number[];
  /** Whether the viewer's own partner is among them. */
  readonly own: boolean;
}

/** An attachment as a message lists it. */
export interface AttachmentData {
  readonly id: number;
  readonly name: string | null;
  readonly mimetype: string | null;
  readonly fileSize?: number | null;
  readonly checksum?: string | null;
  readonly type?: string | null;
  readonly url?: string | null;
}

/** A plain snapshot of one message, read from the store inside the caller's reactive scope. */
export interface MessageData {
  readonly id: number;
  /** The server-sanitized HTML body. */
  readonly body: string;
  readonly messageType: string;
  readonly date: Date;
  readonly authorId: number | null;
  readonly subject: string | null;
  readonly isInternal: boolean;
  readonly parentId: number | null;
  readonly tracking: readonly TrackingChange[];
  readonly attachments: readonly AttachmentData[];
  readonly reactions: readonly ReactionGroup[];
  readonly starred: boolean;
  readonly pinnedAt: Date | null;
  readonly editedAt: Date | null;
}

const capabilityCache = new WeakMap<MessagingStore, { readonly generation: string | null; readonly caps: Capabilities }>();

/** Whether the store's schema declares `model.field` (an untracked probe; a model without a schema entry is false). */
export function declares(store: MessagingStore, model: string, field: string): boolean {
  try {
    untrack(() => store.record(model, 1).get(field));
    return true;
  } catch (error) {
    if (error instanceof MessagingStoreError && (error.code === "unknown_field" || error.code === "unknown_model")) return false;
    throw error;
  }
}

/**
 * The capabilities of the store's current schema generation (cached per generation).
 *
 * @param store - the messaging store
 */
export function capabilitiesOf(store: MessagingStore): Capabilities {
  const generation = store.generation;
  const cached = capabilityCache.get(store);
  if (cached !== undefined && cached.generation === generation) return cached.caps;
  const message = store.config.messageModel;
  const has = (field: string): boolean => declares(store, message, field);
  const attachmentField = has("attachment_ids") ? "attachment_ids" : has("attachments") ? "attachments" : null;
  const caps: Capabilities = Object.freeze({
    subject: has("subject"),
    internal: has("is_internal"),
    parent: has("parent_id"),
    tracking: has("tracking"),
    attachments: attachmentField !== null && declares(store, ATTACHMENT_MODEL, "name"),
    reactions: has("reactions") && declares(store, REACTION_MODEL, "content") && declares(store, REACTION_MODEL, "partner_id"),
    bookmark: has("starred"),
    pin: has("pinned_at"),
    edited: has("edited_at"),
    partnerNames: declares(store, PARTNER_MODEL, "name"),
  });
  capabilityCache.set(store, { generation, caps });
  return caps;
}

/**
 * Refuse a store whose schema lacks a field the components need.
 *
 * @param store - the messaging store
 * @throws {@link ThreadViewError} `schema` naming the missing field and the schema generation
 */
export function assertContract(store: MessagingStore): void {
  const message = store.config.messageModel;
  for (const field of REQUIRED_MESSAGE_FIELDS) {
    if (!declares(store, message, field)) {
      throw new ThreadViewError("schema", `${message}.${field} is not in schema ${store.generation ?? "(none)"}: the thread view needs it`);
    }
  }
}

const DATETIME = /^(\d{4})-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})$/;

/**
 * A wire datetime (`YYYY-MM-DD HH:MM:SS`, UTC) as a Date.
 *
 * @param text - the wire text
 * @param where - what is being read, for the error message
 * @throws {@link ThreadViewError} `data` for any other text
 */
export function parseWireDatetime(text: unknown, where: string): Date {
  const match = typeof text === "string" ? DATETIME.exec(text) : null;
  if (match === null) throw new ThreadViewError("data", `${where} must be a wire datetime (YYYY-MM-DD HH:MM:SS), got ${JSON.stringify(text)}`);
  const [, y, mo, d, h, mi, s] = match;
  const date = new Date(Date.UTC(Number(y), Number(mo) - 1, Number(d), Number(h), Number(mi), Number(s)));
  if (Number.isNaN(date.getTime()) || date.getUTCMonth() !== Number(mo) - 1 || date.getUTCDate() !== Number(d)) {
    throw new ThreadViewError("data", `${where} is not a calendar date: ${text as string}`);
  }
  return date;
}

function optionalDatetime(handle: RecordHandle<string>, field: string, present: boolean, where: string): Date | null {
  if (!present) return null;
  const value = handle.get(field);
  return value === undefined || value === null || value === "" ? null : parseWireDatetime(value, where);
}

const TRACKING_LIMIT = 256;

/**
 * Parse the documented tracking JSON (an array of changes). Unknown keys are ignored; the keys the renderer shows
 * must have their documented types.
 *
 * @param text - the stored JSON text
 * @param where - what is being read, for the error message
 * @throws {@link ThreadViewError} `data` for malformed JSON or a change of the wrong shape
 */
export function parseTracking(text: string, where: string): TrackingChange[] {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw new ThreadViewError("data", `${where} is not valid JSON: ${(error as Error).message}`);
  }
  if (!Array.isArray(value)) throw new ThreadViewError("data", `${where} must be a JSON array of changes`);
  if (value.length > TRACKING_LIMIT) throw new ThreadViewError("data", `${where} lists ${value.length} changes; the limit is ${TRACKING_LIMIT}`);
  return value.map((entry: unknown, index): TrackingChange => {
    if (entry === null || typeof entry !== "object" || Array.isArray(entry)) {
      throw new ThreadViewError("data", `${where}[${index}] must be an object`);
    }
    const change = entry as Record<string, unknown>;
    const name = change["n"];
    const type = change["t"];
    if (typeof name !== "string" || name === "") throw new ThreadViewError("data", `${where}[${index}].n must be the field name`);
    if (typeof type !== "string" || type === "") throw new ThreadViewError("data", `${where}[${index}].t must be the field type`);
    const display = (key: "od" | "vd"): string | null => {
      const shown = change[key];
      if (shown === undefined || shown === null) return null;
      if (typeof shown !== "string") throw new ThreadViewError("data", `${where}[${index}].${key} must be a string or null`);
      return shown;
    };
    return { field: name, type, old: change["o"] ?? null, new: change["v"] ?? null, oldDisplay: display("od"), newDisplay: display("vd") };
  });
}

function stringField(handle: RecordHandle<string>, field: string, where: string): string {
  const value = handle.get(field);
  if (typeof value !== "string") throw new ThreadViewError("data", `${where}.${field} must be text, got ${JSON.stringify(value)}`);
  return value;
}

/**
 * Read one message from the store. Tracked: call it inside an effect to follow the message.
 *
 * @param store - the messaging store
 * @param id - the message id
 * @param selfPartnerId - the viewer's partner id (marks the viewer's own reactions)
 * @returns the snapshot, or `undefined` while the store holds no record of the message
 * @throws {@link ThreadViewError} `data` for a value of the wrong shape
 */
export function readMessage(store: MessagingStore, id: number, selfPartnerId: number | null): MessageData | undefined {
  const caps = capabilitiesOf(store);
  const model = store.config.messageModel;
  const handle: RecordHandle<string> = store.record(model, id);
  const where = `${model}#${id}`;
  const body = handle.get("body");
  if (body === undefined) return undefined;
  if (typeof body !== "string") throw new ThreadViewError("data", `${where}.body must be text, got ${JSON.stringify(body)}`);
  const author = handle.get("author_id");
  if (author !== undefined && author !== null && (typeof author !== "number" || !Number.isSafeInteger(author))) {
    throw new ThreadViewError("data", `${where}.author_id must be a partner id or empty, got ${JSON.stringify(author)}`);
  }
  const parent = caps.parent ? handle.get("parent_id") : null;
  if (parent !== undefined && parent !== null && typeof parent !== "number") {
    throw new ThreadViewError("data", `${where}.parent_id must be a message id or empty, got ${JSON.stringify(parent)}`);
  }
  let subject: string | null = null;
  if (caps.subject) {
    const raw = handle.get("subject");
    if (typeof raw === "string" && raw !== "") subject = raw;
    else if (raw !== undefined && raw !== null && raw !== false && raw !== "") {
      throw new ThreadViewError("data", `${where}.subject must be text, got ${JSON.stringify(raw)}`);
    }
  }
  let tracking: TrackingChange[] = [];
  if (caps.tracking) {
    const raw = handle.get("tracking");
    if (typeof raw === "string" && raw !== "") tracking = parseTracking(raw, `${where}.tracking`);
    else if (raw !== undefined && raw !== null && raw !== "") {
      throw new ThreadViewError("data", `${where}.tracking must be the JSON text of the changes, got ${JSON.stringify(raw)}`);
    }
  }
  const attachmentField = declares(store, model, "attachment_ids")
    ? "attachment_ids"
    : declares(store, model, "attachments")
      ? "attachments"
      : null;
  const attachments: AttachmentData[] = caps.attachments && attachmentField !== null
    ? handle.many(attachmentField as never).map((a) => {
        const name = a.get("name");
        const mimetype = declares(store, ATTACHMENT_MODEL, "mimetype") ? a.get("mimetype") : undefined;
        const fileSize = declares(store, ATTACHMENT_MODEL, "file_size") ? a.get("file_size") : undefined;
        const checksum = declares(store, ATTACHMENT_MODEL, "checksum") ? a.get("checksum") : undefined;
        const type = declares(store, ATTACHMENT_MODEL, "type") ? a.get("type") : undefined;
        const url = declares(store, ATTACHMENT_MODEL, "url") ? a.get("url") : undefined;
        return {
          id: a.id as number,
          name: typeof name === "string" && name !== "" ? name : null,
          mimetype: typeof mimetype === "string" && mimetype !== "" ? mimetype : null,
          fileSize: typeof fileSize === "number" && Number.isFinite(fileSize) ? fileSize : null,
          checksum: typeof checksum === "string" && checksum !== "" ? checksum : null,
          type: typeof type === "string" && type !== "" ? type : null,
          url: typeof url === "string" && url !== "" ? url : null,
        };
      })
    : [];
  const groups = new Map<string, number[]>();
  if (caps.reactions) {
    for (const reaction of handle.many("reactions")) {
      const content = reaction.get("content");
      const partner = reaction.get("partner_id");
      if (typeof content !== "string" || content === "" || typeof partner !== "number") continue;
      const list = groups.get(content);
      if (list === undefined) groups.set(content, [partner]);
      else list.push(partner);
    }
  }
  const reactions: ReactionGroup[] = [...groups].map(([content, partnerIds]) => ({
    content,
    count: partnerIds.length,
    partnerIds,
    own: selfPartnerId !== null && partnerIds.includes(selfPartnerId),
  }));
  const messageType = stringField(handle, "message_type", where);
  return {
    id,
    body,
    messageType,
    date: parseWireDatetime(handle.get("date"), `${where}.date`),
    authorId: typeof author === "number" ? author : null,
    subject,
    isInternal: caps.internal && handle.get("is_internal") === true,
    parentId: typeof parent === "number" ? parent : null,
    tracking,
    attachments,
    reactions,
    starred: caps.bookmark && handle.get("starred") === true,
    pinnedAt: optionalDatetime(handle, "pinned_at", caps.pin, `${where}.pinned_at`),
    editedAt: optionalDatetime(handle, "edited_at", caps.edited, `${where}.edited_at`),
  };
}

/**
 * The name of a partner, or `undefined` when the store holds none (the schema lacks the field or the record is not
 * loaded). Tracked.
 *
 * @param store - the messaging store
 * @param id - the partner id
 */
export function partnerName(store: MessagingStore, id: number): string | undefined {
  if (!capabilitiesOf(store).partnerNames) return undefined;
  const name = store.record(PARTNER_MODEL, id).get("name");
  return typeof name === "string" && name !== "" ? name : undefined;
}

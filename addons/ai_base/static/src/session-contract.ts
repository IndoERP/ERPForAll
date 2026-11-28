/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Client contract for the assistant session panel (board tasks AI1.SESSION-VIEW and
 * AI1.SESSION-LIST.WEB, one bundled slice). Every RPC model name, method name and payload the
 * panel needs lives in this one module. No other ai_base client module may
 * call an `ai.session` method directly, and no field name appears in a call without being
 * declared here.
 *
 * contract choices (reconciled against `session_rpc.rs` in this branch):
 * - model `ai.session` (1), snake_case methods: `search_sessions`, `resume_session`,
 *   `fork_session`, `archive_session`, `session_delete`, `session_export`. `session_events`
 *   (thread rendering) is deliberately absent: the message thread is AI1.ASSIST-TURN's pair slice.
 * - `search_sessions({ search, include_archived, about? })` returns summaries ordered by last
 *   activity; the server matches `search` against the title and the owner's own message text (ADR
 *   0189 D1.2). `about` is the server's own `{model, res_id}` filter,
 *   sent only when the shell context carries an open record; the panel never reorders a response.
 *   `search` and `include_archived` are always sent; `search` is "" when the box is empty.
 * - `resume_session({ id })` returns `{ id, last_seq, anchor }`; `last_seq` is the sequence the
 *   next turn continues (resuming continues its sequence). The payload carries the
 *   id and nothing else: no anchor value ever travels with it.
 * - `fork_session({ id })` forks from the latest summary (5) and returns the new
 *   session as `{ id }`.
 * - `archive_session({ id, state })` with state `open` | `archived`; restore reuses the same
 *   call. The archived session stays listed and readable.
 * - `session_delete({ id })` removes the session through the owner deletion path (2)
 *   and returns `{ id }`. Until AI1.SESSION-RETENTION lands, the server answers every delete with
 *   `erp.ai.session_delete_pending`; the panel maps that to user copy, so this validator stays
 *   unreachable for now (fail-loud, which is correct).
 * - `session_export({ id })` returns `{ format: "json", filename, content }`: the events with
 *   tool results re-read under current rights and model-written text filtered.4;
 *   the panel turns it into a file download.
 * - the anchor object carries identifiers only (`model`, `res_id`) plus the server's `available`
 *   verdict; it never carries a record value (no anchor value reaches the panel).
 *   The panel renders the model and id only when `available` is true, otherwise exactly
 *   "Anchor not available".
 * - the open-record anchor is read from the systray environment context keys `active_model` and
 *   `active_id` (the client's established context key names). The shell publishes them into the
 *   systray context as the mounted record changes (AI1.ANCHOR-CONTEXT.WEB); the object stays live
 *   for the dock's lifetime, and the panel re-reads it whenever it loads or is shown again.
 * - the panel displays only title, state, last activity and anchor; event and turn counts are
 *   not needed by this slice and are not declared.
 */

export interface RpcLike {
  call(model: string, method: string, args: Record<string, unknown>): Promise<unknown>;
}

/** The anchored record as identifiers plus the server's readability verdict; never a value. */
export interface SessionAnchor {
  readonly model: string;
  readonly res_id: number;
  readonly available: boolean;
}

export interface SessionSummary {
  readonly id: number;
  readonly title: string;
  readonly state: "open" | "archived";
  readonly last_event_at: string;
  readonly anchor: SessionAnchor | null;
}

export interface SearchSessionsArgs {
  readonly search?: string;
  readonly include_archived?: boolean;
  /** The open record, as `{model, res_id}`: the server filters to the reader's own sessions anchored there. */
  readonly about?: OpenRecordAnchor;
}

export interface ResumeSessionResult {
  readonly id: number;
  readonly last_seq: number;
  readonly anchor: SessionAnchor | null;
}

export interface ForkSessionResult {
  readonly id: number;
}

export interface ArchiveSessionResult {
  readonly id: number;
  readonly state: "open" | "archived";
}

export interface DeleteSessionResult {
  readonly id: number;
}

export interface SessionExport {
  readonly format: "json";
  readonly filename: string;
  readonly content: string;
}

/** The record currently open in the form, taken from the shell context. */
export interface OpenRecordAnchor {
  readonly model: string;
  readonly res_id: number;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function id(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function text(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function anchor(value: unknown): value is SessionAnchor {
  return record(value) && text(value["model"]) && id(value["res_id"]) && typeof value["available"] === "boolean";
}

function sessionSummary(value: unknown): value is SessionSummary {
  return record(value) && id(value["id"]) && typeof value["title"] === "string"
    && (value["state"] === "open" || value["state"] === "archived") && text(value["last_event_at"])
    && (value["anchor"] === null || anchor(value["anchor"]));
}

/** The open-record anchor of the shell context: `active_model` + `active_id`, or none. */
export function anchorFromContext(context: Record<string, unknown> | undefined): OpenRecordAnchor | null {
  if (!context) return null;
  const model = context["active_model"];
  const resId = context["active_id"];
  if (model === undefined && resId === undefined) return null;
  // One half present without the other, or a malformed value, is a wiring bug: fail loudly.
  if (!text(model) || !id(resId)) {
    throw new Error("The shell context carries an invalid open record.");
  }
  return { model, res_id: resId };
}

export async function searchSessions(rpc: RpcLike, args: SearchSessionsArgs): Promise<SessionSummary[]> {
  // The `about` filter travels only as identifiers and is refused here when malformed, so the
  // server's `erp.ai.about_invalid` never fires from this module.
  let about: OpenRecordAnchor | undefined;
  if (args.about !== undefined) {
    const candidate: unknown = args.about;
    if (!record(candidate)) throw new Error("The open record anchor is malformed.");
    const model: unknown = candidate["model"];
    const resId: unknown = candidate["res_id"];
    if (!text(model) || !id(resId)) throw new Error("The open record anchor is malformed.");
    about = { model, res_id: resId };
  }
  const result = await rpc.call("ai.session", "search_sessions", {
    search: args.search ?? "",
    include_archived: args.include_archived ?? false,
    ...(about === undefined ? {} : { about }),
  });
  if (!Array.isArray(result) || !result.every(sessionSummary)) {
    throw new Error("The session store returned an invalid session list.");
  }
  return result;
}

export async function resumeSession(rpc: RpcLike, sessionId: number): Promise<ResumeSessionResult> {
  const result = await rpc.call("ai.session", "resume_session", { id: sessionId });
  if (!record(result) || !id(result["id"]) || typeof result["last_seq"] !== "number"
      || !Number.isSafeInteger(result["last_seq"]) || result["last_seq"] < 0
      || !(result["anchor"] === null || anchor(result["anchor"]))) {
    throw new Error("The session store returned an invalid resume result.");
  }
  return { id: result["id"], last_seq: result["last_seq"], anchor: result["anchor"] === null ? null : result["anchor"] };
}

export async function forkSession(rpc: RpcLike, sessionId: number): Promise<ForkSessionResult> {
  const result = await rpc.call("ai.session", "fork_session", { id: sessionId });
  if (!record(result) || !id(result["id"])) {
    throw new Error("The session store returned an invalid fork result.");
  }
  return { id: result["id"] };
}

export async function archiveSession(rpc: RpcLike, sessionId: number, state: "open" | "archived"): Promise<ArchiveSessionResult> {
  const result = await rpc.call("ai.session", "archive_session", { id: sessionId, state });
  if (!record(result) || !id(result["id"]) || (result["state"] !== "open" && result["state"] !== "archived")) {
    throw new Error("The session store returned an invalid archive result.");
  }
  return { id: result["id"], state: result["state"] };
}

export async function deleteSession(rpc: RpcLike, sessionId: number): Promise<DeleteSessionResult> {
  const result = await rpc.call("ai.session", "session_delete", { id: sessionId });
  if (!record(result) || !id(result["id"])) {
    throw new Error("The session store returned an invalid delete result.");
  }
  return { id: result["id"] };
}

export async function exportSession(rpc: RpcLike, sessionId: number): Promise<SessionExport> {
  const result = await rpc.call("ai.session", "session_export", { id: sessionId });
  if (!record(result) || result["format"] !== "json" || !text(result["filename"]) || typeof result["content"] !== "string") {
    throw new Error("The session store returned an invalid export.");
  }
  return { format: "json", filename: result["filename"], content: result["content"] };
}

/** The authenticated transcript for the active company. */
export async function conversationHistory(rpc: RpcLike, sessionId: number, companyId: number) {
  const result = await rpc.call("ai.session", "conversation_history", {id:sessionId, context:{allowed_company_ids:[companyId]}});
  if (!record(result) || result["id"] !== sessionId || !Array.isArray(result["messages"])) {
    throw new Error("Invalid saved conversation.");
  }
  return result["messages"] as unknown[];
}

export async function locateSubmission(rpc: RpcLike, request: string, company: number): Promise<number | null> {
  const result = await rpc.call("ai.session", "locate_submission", {request, context:{allowed_company_ids:[company]}});
  if (!record(result)) throw new Error("Invalid submission receipt.");
  const id = result["session_id"];
  if (id === null) return null;
  if (typeof id !== "number" || !Number.isSafeInteger(id) || id <= 0) throw new Error("Invalid submission receipt.");
  return id;
}

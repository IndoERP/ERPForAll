/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/**
 * What kind of problem a {@link ThreadViewError} reports:
 * - `schema`: a field the components require is not in the store's schema;
 * - `unavailable`: a capability or transport operation that does not exist yet; the message names the owning task;
 * - `bad_option`: a component was constructed with a missing or invalid option;
 * - `registry`: the registries were not defined (or defined twice) on the hub;
 * - `data`: a store value breaks the documented shape of the contract (for example malformed tracking JSON);
 * - `transport`: a transport answer breaks its contract;
 * - `mention`: a mention span that does not fit the text it was recorded for;
 * - `body`: a message body over the renderer's caps;
 * - `pending_limit`: too many unsent posts in one thread;
 * - `disposed`: use of a component after `dispose()`.
 */
export type ThreadViewErrorCode =
  | "schema"
  | "unavailable"
  | "bad_option"
  | "registry"
  | "data"
  | "transport"
  | "mention"
  | "body"
  | "pending_limit"
  | "disposed";

/** The error the thread view components throw or report. `code` is a closed list; the message names what was wrong. */
export class ThreadViewError extends Error {
  override readonly name = "ThreadViewError";
  readonly code: ThreadViewErrorCode;

  constructor(code: ThreadViewErrorCode, message: string) {
    super(message);
    this.code = code;
  }
}

/** The tasks that own the server half of each capability (used in `unavailable` messages and hidden-action reasons). */
export const OWNING_TASK = Object.freeze({
  reactions: "a later release",
  bookmark: "a later release",
  pin: "a later release",
  delete: "a later release",
  tracking: "a later release",
  attachments: "a later release",
  upload: "a later release",
  fullComposer: "a later release",
});

/** The standard refusal of a feature whose server half does not exist yet. */
export function unavailable(what: string, task: string): ThreadViewError {
  return new ThreadViewError("unavailable", `${what} is not available until ${task}`);
}

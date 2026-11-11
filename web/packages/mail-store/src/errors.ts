/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/**
 * What kind of problem a {@link MessagingStoreError} reports:
 * - `bad_config`: a cap or name in the store configuration is invalid;
 * - `unknown_model`: a model that is neither in the schema nor a local model;
 * - `unknown_field`: a field the model does not declare;
 * - `local_model_clash`: a local model uses a name that the schema declares (or the schema introduces it later);
 * - `bad_local`: a local record call for a record that is missing or exists, or a local model defined badly;
 * - `not_local`: a local-only operation on a wire model;
 * - `delegation`: an invalid or cyclic delegation rule;
 * - `window`: a message window call that would break contiguity or is made against its state;
 * - `read_only`: a write to a server record's field, delegated or not (payloads are the only writers);
 * - `field_kind`: a relation accessor used on a field of another kind;
 * - `bad_id`: a record id that is invalid for its model (a wire id is a safe integer, a local id a non-empty string);
 * - `request`: a request id that was never begun or already settled;
 * - `no_schema`: an operation that needs a schema before one was set.
 */
export type MessagingStoreErrorCode =
  | "bad_config"
  | "unknown_model"
  | "unknown_field"
  | "local_model_clash"
  | "bad_local"
  | "not_local"
  | "delegation"
  | "window"
  | "read_only"
  | "field_kind"
  | "bad_id"
  | "request"
  | "no_schema";

/** The error the messaging store throws or reports. `code` is a closed list; the message names what was wrong. */
export class MessagingStoreError extends Error {
  override readonly name = "MessagingStoreError";
  readonly code: MessagingStoreErrorCode;

  constructor(code: MessagingStoreErrorCode, message: string) {
    super(message);
    this.code = code;
  }
}

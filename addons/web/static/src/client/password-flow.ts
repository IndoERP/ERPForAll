/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Browser half of the managed password operations.
// Password form fields are never stored: the server refuses an ordinary save of a nonempty secret. A
// submission therefore skips the save and sends the typed secrets as parameters of its fixed operation.
// When an operation answers with the identity confirmation instead, the client keeps that operation and
// sends it again once the identity is confirmed; the server stores no callback and no password.
import { ClientError } from "./transport.js";

/** The submissions whose secrets travel only as operation parameters. */
export type PasswordSubmission = "confirm_identity" | "change_own" | "reset_users";

const submissions: ReadonlyMap<string, PasswordSubmission> = new Map([
  ["res.users.identitycheck/run_check", "confirm_identity"],
  ["change.password.own/change_password", "change_own"],
  ["change.password.wizard/change_password_button", "reset_users"]
]);
/** Operations that may answer with the identity confirmation and are then sent again (item 5). */
const resubmitted: ReadonlySet<string> = new Set([
  "res.users/action_change_password_wizard",
  "res.users/preference_change_password",
  "change.password.own/change_password",
  "change.password.wizard/change_password_button"
]);
/** The transient password models whose form fields hold typed secrets. */
export const passwordModels: ReadonlySet<string> = new Set(["res.users.identitycheck", "change.password.own", "change.password.wizard", "change.password.user"]);
/** The identity confirmation model the server names when a confirmation is required. */
export const IDENTITY_MODEL = "res.users.identitycheck";
/** The identity confirmation's submit button. */
export const IDENTITY_METHOD = "run_check";

/** The password submission `method` of `model` performs, or undefined for every ordinary button. */
export function passwordSubmission(model: string, method: string): PasswordSubmission | undefined {
  return submissions.get(`${model}/${method}`);
}

/** Whether an identity confirmation answering `model.method` sends that same operation again. */
export function passwordResubmitted(model: string, method: string): boolean {
  return resubmitted.has(`${model}/${method}`);
}

/** One operation kept in memory so it can be sent again after the identity confirmation. */
export interface Resubmission {
  readonly model: string;
  readonly method: string;
  call(): Promise<unknown>;
}

/** A typed secret as the operation receives it; an untouched field is the empty string. */
function secret(value: unknown, label: string): string {
  if (value === undefined || value === null || value === false) return "";
  if (typeof value !== "string") throw new ClientError("validation", `${label} must be text.`);
  return value;
}

function wizardId(id: number | undefined): number {
  if (id === undefined) throw new ClientError("validation", "The password form was not opened by the server; close it and open it again.");
  return id;
}

/** One selected-user line of the reset wizard: the server-created line id and the typed password. */
function resetLine(line: unknown): { line_id: number; new_passwd: string } {
  if (typeof line === "number") return { line_id: line, new_passwd: "" };
  const values = line as Record<string, unknown> | null;
  const id = values?.["id"];
  if (typeof id !== "number") throw new ClientError("validation", "Selected users are added by the server; close the wizard and open it again.");
  return { line_id: id, new_passwd: secret(values!["new_passwd"], "New password") };
}

/** The operation parameters of `kind` for the open record: secrets from the typed values, never stored. */
export function passwordArguments(kind: PasswordSubmission, values: Record<string, unknown>, id: number | undefined, context: Record<string, unknown>): Record<string, unknown> {
  switch (kind) {
    // The identity check is a model operation: it names no record.
    case "confirm_identity":
      return { password: secret(values["password"], "Password"), context };
    case "change_own":
      return { ids: [wizardId(id)], new_password: secret(values["new_password"], "New password"),
        confirm_password: secret(values["confirm_password"], "Confirm password"), context };
    case "reset_users": {
      const lines = values["user_ids"];
      if (!Array.isArray(lines)) throw new ClientError("validation", "The reset wizard holds no selected users.");
      return { ids: [wizardId(id)], changes: lines.map(resetLine), context };
    }
  }
}

/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import type { Field } from "./metadata.js";
import { t } from "./translate.js";
import { ClientError } from "./transport.js";
import type { PasswordSubmission } from "./password-flow.js";

/** Keep the label associated with the input and the reveal button inside its right edge. */
export function wrapPassword(input: HTMLInputElement, label: string): void {
  const frame = el("div", undefined, "managed-password-control");
  const reveal = button("", () => {
    const visible = input.type === "password";
    input.type = visible ? "text" : "password";
    reveal.setAttribute("aria-pressed", String(visible));
  }, "managed-password-reveal");
  reveal.setAttribute("aria-label", t("Show {label}", { label }));
  reveal.setAttribute("aria-controls", input.id);
  reveal.setAttribute("aria-pressed", "false");
  const eye = el("span", undefined, "password-eye");
  eye.setAttribute("aria-hidden", "true");
  reveal.append(eye);
  input.replaceWith(frame);
  frame.append(input, reveal);
}

/** Server-created reset lines have fixed targets. Only the new password is editable. */
export function passwordResetRows(lines: readonly Record<string, unknown>[], fields: readonly Field[]) {
  const password = fields.find(field => field.name === "new_passwd" && !field.readonly);
  const loginField = fields.find(field => field.name === "user_login");
  if (!password || !loginField || !lines.length) throw new ClientError("validation", "The selected password reset accounts are unavailable.");
  const node = el("div", undefined, "password-reset-rows");
  for (const line of lines) {
    const login = line["user_login"], id = line["id"];
    if (typeof login !== "string" || typeof id !== "number") throw new ClientError("server", "A selected password reset account is missing its login or identity.");
    const group = el("fieldset", undefined, "password-reset-target");
    const account = line["user_id"];
    const name = account && typeof account === "object" && "display_name" in account && typeof account.display_name === "string" ? account.display_name : "";
    const legend = el("legend", name && name !== login ? t("{name} ({login})", { name, login }) : login);
    const input = el("input");
    input.type = "password";
    input.name = `new_passwd_${id}`;
    input.autocomplete = `section-reset${id} new-password`;
    input.required = lines.length === 1;
    const shell = fieldShell({ ...password, required: input.required }, input);
    wrapPassword(input, password.label);
    group.append(legend, shell.node);
    node.append(group);
  }
  if (lines.length > 1) node.prepend(el("p", t("Leave a password blank to keep it unchanged.")));
  return { node, dispose: () => { for (const input of node.querySelectorAll("input")) input.value = ""; } };
}

/** Password managers and keyboard submission can bypass the scalar widget's blur/change event. */
export function passwordFormValues(kind: PasswordSubmission, form: HTMLFormElement, values: Record<string, unknown>): Record<string, unknown> {
  function read(name: string): string {
    const field = form.elements.namedItem(name);
    if (!(field instanceof HTMLInputElement)) throw new ClientError("validation", "A password input is missing. Reopen this dialog.");
    return field.value;
  }
  if (kind === "confirm_identity") return { password: read("password") };
  if (kind === "change_own") return { new_password: read("new_password"), confirm_password: read("confirm_password") };
  const rows = values["user_ids"];
  if (!Array.isArray(rows)) throw new ClientError("validation", "The reset wizard holds no selected users.");
  return { user_ids: rows.map(line => ({ id: line.id, new_passwd: read(`new_passwd_${line.id}`) })) };
}

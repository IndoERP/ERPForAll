/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { passwordArguments, passwordResubmitted, passwordSubmission, type Resubmission } from "../src/client/password-flow.js";
import { runObjectButton, type ButtonEnvironment } from "../src/client/buttons.js";
import type { ButtonNode } from "../src/client/metadata.js";
import { RecordState } from "../src/client/record.js";
import { ClientError, type RpcClient } from "../src/client/transport.js";

const context = { lang: "en_US" };

/// (partial: browser half; the closed set of submissions the client sends as managed operations)
test("only the three password submissions skip the save, and only fixed operations are resubmitted", () => {
  expect(passwordSubmission("res.users.identitycheck", "run_check")).toBe("confirm_identity");
  expect(passwordSubmission("change.password.own", "change_password")).toBe("change_own");
  expect(passwordSubmission("change.password.wizard", "change_password_button")).toBe("reset_users");
  expect(passwordSubmission("res.users", "action_change_password_wizard")).toBeUndefined();
  expect(passwordSubmission("test.book", "run_check")).toBeUndefined();
  expect(passwordResubmitted("res.users", "action_change_password_wizard")).toBe(true);
  expect(passwordResubmitted("res.users", "preference_change_password")).toBe(true);
  expect(passwordResubmitted("res.users.identitycheck", "run_check")).toBe(false);
  expect(passwordResubmitted("res.users", "preference_save")).toBe(false);
});

/// (partial: browser half; the identity check names no record and carries only the typed password)
test("the identity confirmation sends the typed password without record ids", () => {
  expect(passwordArguments("confirm_identity", { password: "typed-secret" }, undefined, context))
    .toEqual({ password: "typed-secret", context });
  expect(passwordArguments("confirm_identity", { password: false }, undefined, context)).toEqual({ password: "", context });
});

/// (partial: browser half; the own form's new password and confirmation are sent as parameters)
test("the own password form sends its wizard id with both typed values", () => {
  expect(passwordArguments("change_own", { new_password: "a", confirm_password: "b" }, 9, context))
    .toEqual({ ids: [9], new_password: "a", confirm_password: "b", context });
  expect(() => passwordArguments("change_own", { new_password: "a" }, undefined, context)).toThrow(/opened by the server/);
});

/// (partial: browser half; one change per server-created line, untouched lines empty so the server skips them)
test("the reset wizard sends one change per selected-user line", () => {
  const values = { user_ids: [{ id: 31, user_login: "target", new_passwd: "next" }, { id: 32, user_login: "other", new_passwd: false }, 33] };
  expect(passwordArguments("reset_users", values, 7, context)).toEqual({ ids: [7], context, changes: [
    { line_id: 31, new_passwd: "next" }, { line_id: 32, new_passwd: "" }, { line_id: 33, new_passwd: "" }
  ] });
  expect(() => passwordArguments("reset_users", { user_ids: [{ user_login: "added", new_passwd: "x" }] }, 7, context)).toThrow(/added by the server/);
  expect(() => passwordArguments("reset_users", { user_ids: [{ id: 31, new_passwd: 5 }] }, 7, context)).toThrow(ClientError);
  expect(() => passwordArguments("reset_users", {}, 7, context)).toThrow(/no selected users/);
});

const node = (name: string): ButtonNode => ({ kind: "button", name, label: name, variant: "primary", confirm: null, invisible: null });
const environment = (model: string, extra: Partial<ButtonEnvironment> = {}) => {
  const log: unknown[] = [];
  const env: ButtonEnvironment = {
    model, context, id: () => 9, dirty: () => true,
    save: vi.fn(async () => { log.push("save"); }),
    call: vi.fn(async (m: string, method: string, args: Record<string, unknown>) => { log.push([m, method, args]); return { type: "ir.actions.act_window_close" }; }),
    reload: vi.fn(async () => {}),
    confirm: async () => true,
    ...extra
  };
  return { env, log };
};

/// (partial: browser half; a dirty password form is never saved, its secrets go only to the operation)
test("a password submission skips the save even when the form is dirty", async () => {
  const results: unknown[] = [];
  const { env, log } = environment("change.password.own", {
    secretArguments: async (method, effective) => method === "change_password" ? { ids: [9], new_password: "a", confirm_password: "a", context: effective() } : undefined,
    result: async (value, effective, resubmit) => { results.push([value, effective, resubmit?.method]); }
  });
  await runObjectButton(node("change_password"), env);
  expect(log).toEqual([["change.password.own", "change_password", { ids: [9], new_password: "a", confirm_password: "a", context }]]);
  expect(env.save).not.toHaveBeenCalled();
  expect(results).toEqual([[{ type: "ir.actions.act_window_close" }, undefined, "change_password"]]);
});

/// (partial: browser half; a refused form validation sends nothing)
test("a password submission that its form refuses sends nothing", async () => {
  const { env, log } = environment("change.password.own", { secretArguments: async () => null, result: vi.fn(async () => {}) });
  await runObjectButton(node("change_password"), env);
  expect(log).toEqual([]);
  expect(env.result).not.toHaveBeenCalled();
});

/// (partial: browser half; the kept operation is sent again unchanged, through the resubmission call)
test("a resubmittable operation hands its result a kept copy that repeats the exact call", async () => {
  let kept: Resubmission | undefined;
  const again = vi.fn(async () => "again");
  const { env, log } = environment("res.users", { dirty: () => false, resubmitCall: again,
    secretArguments: async () => undefined, result: async (_value, _effective, resubmit) => { kept = resubmit; } });
  await runObjectButton(node("action_change_password_wizard"), env);
  expect(log).toEqual([["res.users", "action_change_password_wizard", { ids: [9], context }]]);
  expect(kept?.model).toBe("res.users");
  expect(await kept!.call()).toBe("again");
  expect(again).toHaveBeenCalledWith("res.users", "action_change_password_wizard", { ids: [9], context });
  // ordinary buttons keep their two-argument result call
  const plain = vi.fn(async () => {});
  await runObjectButton(node("lend"), environment("test.book", { dirty: () => false, result: plain }).env);
  expect(plain).toHaveBeenCalledWith({ type: "ir.actions.act_window_close" });
});

/// (partial: browser half; the record state refuses to save any of the four password models)
test("a password model's record state refuses a generic save before any request", async () => {
  const rpc = { call: vi.fn() } as unknown as RpcClient;
  for (const model of ["res.users.identitycheck", "change.password.own", "change.password.wizard", "change.password.user"]) {
    await expect(new RecordState(rpc, model, [], {}).save()).rejects.toThrow(/never saved/);
  }
  expect(rpc.call).not.toHaveBeenCalled();
});
